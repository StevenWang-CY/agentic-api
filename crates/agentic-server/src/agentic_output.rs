const BLUE: &str = "\u{1b}[38;5;75m";
const GOLD: &str = "\u{1b}[38;5;214m";
const DIM: &str = "\u{1b}[2m";
const BOLD: &str = "\u{1b}[1m";
const RESET: &str = "\u{1b}[0m";

#[must_use]
pub fn render_banner(color: bool) -> String {
    let inner_width = 30;
    let rows = ["  ⚡  Agentic API", "      Local agent gateway"];
    let mut lines = Vec::with_capacity(rows.len() + 2);
    if color {
        lines.push(format!("{DIM}┌{}┐{RESET}", "─".repeat(inner_width)));
    } else {
        lines.push(format!("┌{}┐", "─".repeat(inner_width)));
    }
    lines.extend(rows.into_iter().map(|row| {
        let padding = inner_width.saturating_sub(display_width(row));
        let content = if color {
            match row {
                "  ⚡  Agentic API" => format!("  {GOLD}⚡{RESET}  {BLUE}{BOLD}Agentic{RESET} {GOLD}{BOLD}API{RESET}"),
                _ => format!("{BLUE}{row}{RESET}"),
            }
        } else {
            row.to_owned()
        };
        if color {
            format!("{DIM}│{RESET}{content}{}{DIM}│{RESET}", " ".repeat(padding))
        } else {
            format!("│{content}{}│", " ".repeat(padding))
        }
    }));
    if color {
        lines.push(format!("{DIM}└{}┘{RESET}", "─".repeat(inner_width)));
    } else {
        lines.push(format!("└{}┘", "─".repeat(inner_width)));
    }
    lines.join("\n")
}

#[must_use]
pub fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let suffix_start = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, suffix) = rest.split_at(suffix_start);
    let Some((userinfo, host)) = authority.split_once('@') else {
        return url.to_owned();
    };
    let Some((username, _password)) = userinfo.split_once(':') else {
        return url.to_owned();
    };
    format!("{scheme}://{username}:[REDACTED]@{host}{suffix}")
}

fn display_width(value: &str) -> usize {
    value
        .chars()
        .map(|character| usize::from(character != '\u{fe0f}'))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::{redact_url, render_banner};

    #[test]
    fn banner_rows_have_equal_display_width() {
        let banner = render_banner(false);
        let widths: Vec<_> = banner.lines().map(display_width).collect();
        assert!(widths.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn banner_can_be_colored() {
        let banner = render_banner(true);
        assert!(banner.contains("\u{1b}[38;5;75m"));
        assert!(banner.contains("\u{1b}[38;5;214m"));
        assert!(!render_banner(false).contains('\u{1b}'));
    }

    #[test]
    fn redact_url_hides_password() {
        assert_eq!(
            redact_url("postgresql://alice:secret@db.example/agentic"),
            "postgresql://alice:[REDACTED]@db.example/agentic"
        );
        assert_eq!(
            redact_url("postgresql://alice:secret@db.example"),
            "postgresql://alice:[REDACTED]@db.example"
        );
        assert_eq!(
            redact_url("postgresql://alice:secret@db.example?sslmode=require"),
            "postgresql://alice:[REDACTED]@db.example?sslmode=require"
        );
    }

    fn display_width(value: &str) -> usize {
        value.chars().count()
    }
}
