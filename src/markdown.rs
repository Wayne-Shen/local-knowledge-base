use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd, html};

pub fn render(markdown: &str) -> String {
    let options = Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH;
    let events = Parser::new_ext(markdown, options).filter_map(|event| match event {
        // Model output may quote untrusted source text. Only the parser can create markup.
        Event::Html(text) | Event::InlineHtml(text) => Some(Event::Text(text)),
        Event::Start(Tag::Link { .. } | Tag::Image { .. })
        | Event::End(TagEnd::Link | TagEnd::Image) => None,
        event => Some(event),
    });
    let mut rendered = String::new();
    html::push_html(&mut rendered, events);
    rendered
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn renders_tables_and_keeps_citation_markers() {
        let html =
            render("**Meals** [2]\n\n| Day | Lunch |\n| --- | --- |\n| 50 | Mushroom rice |");
        assert!(html.contains("<strong>Meals</strong> [2]"));
        assert!(html.contains("<table>"));
        assert!(html.contains("<th>Lunch</th>"));
        assert!(html.contains("<td>Mushroom rice</td>"));
    }

    #[test]
    fn escapes_html_and_drops_model_supplied_destinations() {
        let html = render(
            "<img src=x onerror=alert(1)>\n\n[Label](javascript:alert%281%29) ![Alt](https://example.com/private)",
        );
        assert!(html.contains("&lt;img"));
        assert!(html.contains("Label Alt"));
        assert!(!html.contains("<a "));
        assert!(!html.contains("<img "));
        assert!(!html.contains("example.com"));
    }
}
