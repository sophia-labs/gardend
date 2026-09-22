use crate::runtime_config::MDOC_NS;

pub(super) fn node_type_uri(tag_name: &str) -> String {
    let local_name = match tag_name {
        "fragment" => "XmlFragment",
        "paragraph" => "Paragraph",
        "heading" => "Heading",
        "bulletList" => "BulletList",
        "orderedList" => "OrderedList",
        "listItem" => "ListItem",
        "taskList" => "TaskList",
        "taskItem" => "TaskItem",
        "blockquote" => "Blockquote",
        "codeBlock" => "CodeBlock",
        "horizontalRule" => "HorizontalRule",
        "image" => "ImageBlock",
        "strong" | "bold" => "Strong",
        "em" | "italic" => "Emphasis",
        "s" | "strike" => "Strikethrough",
        "code" => "Code",
        "u" | "underline" => "Underline",
        "a" | "link" => "Link",
        "mark" | "highlight" => "Highlight",
        "span" => "Span",
        "textStyle" => "TextStyle",
        "footnote" => "FootnoteMark",
        "commentMark" => "CommentMark",
        "wireMark" => "WireMark",
        "wikilink" => "WikiLink",
        "mathInline" => "MathInline",
        "mathBlock" => "MathBlock",
        "table" => "Table",
        "tableRow" => "TableRow",
        "tableHeader" => "TableHeader",
        "tableCell" => "TableCell",
        "hardBreak" => "HardBreak",
        "text" => "TextNode",
        other => return format!("{MDOC_NS}{}", upper_camel(other)),
    };
    format!("{MDOC_NS}{local_name}")
}

fn upper_camel(value: &str) -> String {
    let mut result = String::new();
    let mut capitalize_next = true;
    for character in value.chars() {
        if !character.is_ascii_alphanumeric() {
            capitalize_next = true;
            continue;
        }
        if capitalize_next {
            result.push(character.to_ascii_uppercase());
            capitalize_next = false;
        } else {
            result.push(character);
        }
    }
    if result.is_empty() {
        "Node".to_string()
    } else {
        result
    }
}
