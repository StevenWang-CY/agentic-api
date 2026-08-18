const CYAN: &str = "\u{1b}[36m";
const RESET: &str = "\u{1b}[0m";

#[must_use]
pub fn render_banner(color: bool) -> String {
    let inner_width = 30;
    let rows = ["  ⚡  Agentic API", "      Local agent gateway"];
    let mut lines = Vec::with_capacity(rows.len() + 2);
    lines.push(format!("┌{}┐", "─".repeat(inner_width)));
    lines.extend(rows.into_iter().map(|row| {
        let padding = inner_width.saturating_sub(display_width(row));
        format!("│{row}{}│", " ".repeat(padding))
    }));
    lines.push(format!("└{}┘", "─".repeat(inner_width)));
    let banner = lines.join("\n");
    if color {
        format!("{CYAN}{banner}{RESET}")
    } else {
        banner
    }
}

#[must_use]
pub fn redact_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let Some((authority, suffix)) = rest.split_once('/') else {
        return url.to_owned();
    };
    let Some((userinfo, host)) = authority.split_once('@') else {
        return url.to_owned();
    };
    let Some((username, _password)) = userinfo.split_once(':') else {
        return url.to_owned();
    };
    format!("{scheme}://{username}:[REDACTED]@{host}/{suffix}")
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
        assert!(render_banner(true).contains("\u{1b}["));
        assert!(!render_banner(false).contains('\u{1b}'));
    }

    #[test]
    fn redact_url_hides_password() {
        assert_eq!(
            redact_url("postgresql://alice:secret@db.example/agentic"),
            "postgresql://alice:[REDACTED]@db.example/agentic"
        );
    }

    fn display_width(value: &str) -> usize {
        value.chars().count()
    }
}
