/*
 * SPDX-FileCopyrightText: 2026 Stalwart contributors
 *
 * SPDX-License-Identifier: AGPL-3.0-only
 *
 * Alert content templates: text interleaved with `%{metric}%` placeholders
 * that are interpolated with live metric values at render time.
 */

/// A parsed alert template: a sequence of literal and metric tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlertContent(Vec<AlertToken>);

#[derive(Debug, Clone, PartialEq, Eq)]
enum AlertToken {
    Text(String),
    Metric(String),
}

impl AlertContent {
    /// Parses a template, recognising `%{name}%` metric placeholders. Anything
    /// that does not form a complete placeholder is kept as literal text.
    pub fn parse(template: &str) -> Self {
        let mut tokens = Vec::new();
        let mut buf = String::new();
        let mut chars = template.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '%' && chars.peek() == Some(&'{') {
                chars.next(); // consume '{'
                let mut name = String::new();
                let mut closed = false;
                for c in chars.by_ref() {
                    if c == '}' {
                        closed = true;
                        break;
                    }
                    name.push(c);
                }

                if closed && chars.peek() == Some(&'%') {
                    chars.next(); // consume trailing '%'
                    if !buf.is_empty() {
                        tokens.push(AlertToken::Text(std::mem::take(&mut buf)));
                    }
                    tokens.push(AlertToken::Metric(name));
                } else {
                    // Not a well-formed placeholder: keep it literally.
                    buf.push_str("%{");
                    buf.push_str(&name);
                    if closed {
                        buf.push('}');
                    }
                }
            } else {
                buf.push(ch);
            }
        }

        if !buf.is_empty() {
            tokens.push(AlertToken::Text(buf));
        }

        AlertContent(tokens)
    }

    /// Renders the template, resolving each metric through `metrics`. An
    /// unresolved metric is rendered as its literal `%{name}%` placeholder.
    pub fn render(&self, metrics: &impl Fn(&str) -> Option<String>) -> String {
        let mut out = String::new();
        for token in &self.0 {
            match token {
                AlertToken::Text(text) => out.push_str(text),
                AlertToken::Metric(name) => match metrics(name) {
                    Some(value) => out.push_str(&value),
                    None => {
                        out.push_str("%{");
                        out.push_str(name);
                        out.push_str("}%");
                    }
                },
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn provider(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name: &str| map.get(name).cloned()
    }

    #[test]
    fn given_a_template_with_a_placeholder_when_rendered_then_the_metric_is_interpolated() {
        let content = AlertContent::parse("cpu load is %{cpu}% now");

        let out = content.render(&provider(&[("cpu", "90")]));

        assert_eq!(out, "cpu load is 90 now");
    }

    #[test]
    fn given_plain_text_when_rendered_then_it_is_unchanged() {
        let content = AlertContent::parse("all systems nominal");

        assert_eq!(content.render(&provider(&[])), "all systems nominal");
    }

    #[test]
    fn given_an_unknown_metric_when_rendered_then_the_placeholder_is_kept() {
        let content = AlertContent::parse("queue is %{queue_size}%");

        assert_eq!(content.render(&provider(&[])), "queue is %{queue_size}%");
    }

    #[test]
    fn given_multiple_placeholders_when_rendered_then_all_are_interpolated() {
        let content = AlertContent::parse("%{a}% and %{b}%");

        let out = content.render(&provider(&[("a", "1"), ("b", "2")]));

        assert_eq!(out, "1 and 2");
    }

    #[test]
    fn given_a_malformed_placeholder_when_rendered_then_it_is_kept_literally() {
        // Missing trailing '%' -> not a placeholder.
        let content = AlertContent::parse("literal %{oops} text");

        assert_eq!(content.render(&provider(&[("oops", "X")])), "literal %{oops} text");
    }
}
