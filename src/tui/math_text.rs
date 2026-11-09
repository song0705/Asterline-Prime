//! Conservative LaTeX-to-Unicode display for ordinary terminal text.
//! This is a bounded notation converter, not an evaluator or a TeX engine.
//! Unsupported input is rejected as a whole so the caller can show the source.

const MAX_INPUT_BYTES: usize = 16 * 1024;
const MAX_DEPTH: usize = 32;

pub(crate) fn to_unicode(source: &str) -> Option<String> {
    if source.len() > MAX_INPUT_BYTES || source.trim().is_empty() {
        return None;
    }
    let mut parser = Parser { source, at: 0 };
    let text = parser.sequence(None, 0)?;
    // Keep ordinary currency/prose pairs from being silently treated as math.
    if !source.contains(['\\', '_', '^', '=', '+', '-', '/', '<', '>'])
        && source.split_whitespace().count() > 1
    {
        return None;
    }
    Some(text.split_whitespace().collect::<Vec<_>>().join(" "))
}

struct Parser<'a> {
    source: &'a str,
    at: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<char> {
        self.source[self.at..].chars().next()
    }

    fn take(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.at += ch.len_utf8();
        Some(ch)
    }

    fn skip_space(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.take();
        }
    }

    fn sequence(&mut self, end: Option<char>, depth: usize) -> Option<String> {
        if depth > MAX_DEPTH {
            return None;
        }
        let mut out = String::new();
        while let Some(ch) = self.peek() {
            if Some(ch) == end {
                self.take();
                return Some(out.trim().to_string());
            }
            if ch.is_whitespace() {
                self.take();
                out.push(' ');
                continue;
            }
            let atom = self.atom(depth + 1)?;
            out.push_str(&atom);
            // A script is attached to the preceding atom. Keep the whole
            // exponent/index when even one character lacks a Unicode glyph.
            let mut sub = false;
            let mut sup = false;
            loop {
                let before_space = self.at;
                self.skip_space();
                let mark = match self.peek() {
                    Some('_') if !sub => {
                        sub = true;
                        '_'
                    }
                    Some('^') if !sup => {
                        sup = true;
                        '^'
                    }
                    Some('_' | '^') => return None,
                    _ => {
                        self.at = before_space;
                        break;
                    }
                };
                self.take();
                let value = self.argument(depth + 1)?;
                if value.is_empty() {
                    return None;
                }
                let text = script(&value, mark == '^').unwrap_or_else(|| {
                    if mark == '^' {
                        format!("^({value})")
                    } else {
                        format!("[{value}]")
                    }
                });
                while out.ends_with(' ') {
                    out.pop();
                }
                out.push_str(&text);
            }
        }
        end.is_none().then(|| out.trim().to_string())
    }

    fn argument(&mut self, depth: usize) -> Option<String> {
        self.skip_space();
        self.atom(depth)
    }

    fn atom(&mut self, depth: usize) -> Option<String> {
        if depth > MAX_DEPTH {
            return None;
        }
        match self.take()? {
            '{' => self.sequence(Some('}'), depth + 1),
            '}' | '_' | '^' | '&' | '#' | '%' | '$' => None,
            '\\' => self.command(depth + 1),
            '+' => Some(" + ".into()),
            '-' => Some(" − ".into()),
            '=' => Some(" = ".into()),
            '<' => Some(" < ".into()),
            '>' => Some(" > ".into()),
            '~' => Some(" ".into()),
            ch if ch.is_control() => None,
            ch => Some(ch.to_string()),
        }
    }

    fn command(&mut self, depth: usize) -> Option<String> {
        let start = self.at;
        while self.peek().is_some_and(|ch| ch.is_ascii_alphabetic()) {
            self.take();
        }
        let name = &self.source[start..self.at];
        if name.is_empty() {
            return match self.take()? {
                ',' | ';' | ':' | ' ' => Some(" ".into()),
                '!' => Some(String::new()),
                '|' => Some("‖".into()),
                '{' => Some("{".into()),
                '}' => Some("}".into()),
                '%' => Some("%".into()),
                '_' => Some("_".into()),
                _ => None,
            };
        }
        if let Some(symbol) = symbol(name) {
            return Some(symbol.into());
        }
        match name {
            "frac" | "dfrac" | "tfrac" => {
                let numerator = self.argument(depth + 1)?;
                let denominator = self.argument(depth + 1)?;
                if numerator.is_empty() || denominator.is_empty() {
                    return None;
                }
                Some(format!(
                    "({}/{})",
                    fraction_part(&numerator),
                    fraction_part(&denominator)
                ))
            }
            "sqrt" => {
                self.skip_space();
                let degree = if self.peek() == Some('[') {
                    self.take();
                    script(&self.sequence(Some(']'), depth + 1)?, true)?
                } else {
                    String::new()
                };
                let body = self.argument(depth + 1)?;
                if body.is_empty() {
                    return None;
                }
                Some(format!("{degree}√({body})"))
            }
            "left" | "right" | "big" | "Big" | "bigg" | "Bigg" => {
                self.skip_space();
                match self.take()? {
                    '.' => Some(String::new()),
                    ch @ ('(' | ')' | '[' | ']' | '|') => Some(ch.to_string()),
                    '\\' => {
                        let delimiter = self.command(depth + 1)?;
                        matches!(
                            delimiter.as_str(),
                            "‖" | "|" | "{" | "}" | "⟨" | "⟩" | "⌈" | "⌉" | "⌊" | "⌋"
                        )
                        .then_some(delimiter)
                    }
                    _ => None,
                }
            }
            "text" | "mathrm" | "operatorname" => {
                self.skip_space();
                if self.take()? != '{' {
                    return None;
                }
                let start = self.at;
                while let Some(ch) = self.take() {
                    match ch {
                        '}' => return Some(self.source[start..self.at - 1].to_string()),
                        '{' | '\\' => return None,
                        _ => {}
                    }
                }
                None
            }
            "mathbb" => self
                .argument(depth + 1)?
                .chars()
                .map(|ch| match ch {
                    'R' => Some('ℝ'),
                    'N' => Some('ℕ'),
                    'Z' => Some('ℤ'),
                    'Q' => Some('ℚ'),
                    'C' => Some('ℂ'),
                    'P' => Some('ℙ'),
                    'H' => Some('ℍ'),
                    _ => None,
                })
                .collect(),
            "hat" | "bar" | "vec" | "tilde" | "dot" | "ddot" => {
                let base = self.argument(depth + 1)?;
                if base.chars().count() != 1 {
                    return None;
                }
                let accent = match name {
                    "hat" => '\u{302}',
                    "bar" => '\u{304}',
                    "vec" => '\u{20d7}',
                    "tilde" => '\u{303}',
                    "dot" => '\u{307}',
                    _ => '\u{308}',
                };
                Some(format!("{base}{accent}"))
            }
            "sin" | "cos" | "tan" | "log" | "ln" | "exp" | "max" | "min" | "argmax" | "argmin"
            | "lim" | "det" | "Pr" => Some(format!("{name} ")),
            _ => None,
        }
    }
}

fn fraction_part(text: &str) -> String {
    if text.chars().all(|ch| ch.is_alphanumeric() || ch == '.') {
        text.to_string()
    } else {
        format!("({text})")
    }
}

fn script(text: &str, superscript: bool) -> Option<String> {
    if text.trim().is_empty() {
        return None;
    }
    let (plain, glyphs) = if superscript {
        (
            "0123456789+-−=()abcdefghijklmnoprstuvwxyzABDEGHIJKLMNOPRTUVWβγδφχ",
            "⁰¹²³⁴⁵⁶⁷⁸⁹⁺⁻⁻⁼⁽⁾ᵃᵇᶜᵈᵉᶠᵍʰⁱʲᵏˡᵐⁿᵒᵖʳˢᵗᵘᵛʷˣʸᶻᴬᴮᴰᴱᴳᴴᴵᴶᴷᴸᴹᴺᴼᴾᴿᵀᵁⱽᵂᵝᵞᵟᵠᵡ",
        )
    } else {
        (
            "0123456789+-−=()aehijklmnoprstuvxβγρφχ",
            "₀₁₂₃₄₅₆₇₈₉₊₋₋₌₍₎ₐₑₕᵢⱼₖₗₘₙₒₚᵣₛₜᵤᵥₓᵦᵧᵨᵩᵪ",
        )
    };
    text.chars()
        .filter(|ch| !ch.is_whitespace())
        .map(|ch| {
            plain
                .chars()
                .zip(glyphs.chars())
                .find_map(|(from, to)| (ch == from).then_some(to))
        })
        .collect()
}

fn symbol(name: &str) -> Option<&'static str> {
    Some(match name {
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ϵ",
        "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "vartheta" => "ϑ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "pi" => "π",
        "varpi" => "ϖ",
        "rho" => "ρ",
        "varrho" => "ϱ",
        "sigma" => "σ",
        "varsigma" => "ς",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" => "ϕ",
        "varphi" => "φ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        "cdot" => " · ",
        "times" => " × ",
        "div" => " ÷ ",
        "pm" => " ± ",
        "mp" => " ∓ ",
        "le" | "leq" => " ≤ ",
        "ge" | "geq" => " ≥ ",
        "ne" | "neq" => " ≠ ",
        "approx" => " ≈ ",
        "sim" => " ∼ ",
        "equiv" => " ≡ ",
        "in" => " ∈ ",
        "notin" => " ∉ ",
        "subset" => " ⊂ ",
        "subseteq" => " ⊆ ",
        "supset" => " ⊃ ",
        "supseteq" => " ⊇ ",
        "cup" => " ∪ ",
        "cap" => " ∩ ",
        "setminus" => " ∖ ",
        "emptyset" | "varnothing" => "∅",
        "forall" => "∀",
        "exists" => "∃",
        "to" | "rightarrow" => " → ",
        "leftarrow" => " ← ",
        "leftrightarrow" => " ↔ ",
        "Rightarrow" | "implies" => " ⇒ ",
        "Leftarrow" => " ⇐ ",
        "Leftrightarrow" | "iff" => " ⇔ ",
        "mapsto" => " ↦ ",
        "infty" => "∞",
        "partial" => "∂",
        "nabla" => "∇",
        "sum" => "∑",
        "prod" => "∏",
        "int" => "∫",
        "iint" => "∬",
        "oint" => "∮",
        "lVert" | "rVert" | "Vert" => "‖",
        "lvert" | "rvert" | "vert" => "|",
        "langle" => "⟨",
        "rangle" => "⟩",
        "lceil" => "⌈",
        "rceil" => "⌉",
        "lfloor" => "⌊",
        "rfloor" => "⌋",
        "ldots" | "dots" => "…",
        "cdots" => "⋯",
        "vdots" => "⋮",
        "ddots" => "⋱",
        "quad" | "qquad" => " ",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_distance_formula_preserves_operators_and_scripts() {
        assert_eq!(to_unicode(r"d_\beta^2 = \beta_t\cdot\frac{1}{3}|\Delta\psi^A|_2^2 + (1-\beta_t)(\Delta\psi^P)^2").as_deref(),
            Some("dᵦ² = βₜ · (1/3)|Δψᴬ|₂² + (1 − βₜ)(Δψᴾ)²"));
    }

    #[test]
    fn fractions_roots_and_composite_scripts_keep_their_scope() {
        for (source, expected) in [
            (r"\frac{a+b}{c+d}^2", "((a + b)/(c + d))²"),
            (r"\sqrt[3]{x+1}", "³√(x + 1)"),
            (r"x_{i,j}^{n+1}", "x[i,j]ⁿ⁺¹"),
            (r"\left\|x\right\|_2", "‖x‖₂"),
            (r"x\in\mathbb{R}", "x ∈ ℝ"),
            (r"\sum_{i=1}^{n}x_i", "∑ᵢ₌₁ⁿxᵢ"),
        ] {
            assert_eq!(to_unicode(source).as_deref(), Some(expected), "{source}");
        }
    }

    #[test]
    fn unsupported_or_truncated_input_is_rejected_without_partial_conversion() {
        for source in [
            r"\frac{a}{",
            r"\unknown{x}",
            r"\begin{matrix}a&b\end{matrix}",
            "x^^2",
            "{x",
            "x}",
            "5 and ",
            r"\mathbf{x}",
        ] {
            assert_eq!(to_unicode(source), None, "{source}");
        }
        assert_eq!(to_unicode(&"{".repeat(100)), None);
        assert_eq!(to_unicode(&"x".repeat(MAX_INPUT_BYTES + 1)), None);
    }
}
