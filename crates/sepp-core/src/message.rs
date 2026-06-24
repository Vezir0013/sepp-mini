//! Nachrichten- und Inhalts-Typen. Die `ContentBlock`-Tags entsprechen bewusst dem
//! Anthropic-Block-Format (`text`/`thinking`/`tool_use`/`tool_result`/`image`), damit
//! die Provider-Serialisierung schlank bleibt.

use serde::{Deserialize, Serialize};

/// Rolle einer Nachricht. `Tool`-Ergebnisse werden als `User`-Nachricht mit
/// `ContentBlock::ToolResult` transportiert (Anthropic kennt keine `tool`-Rolle).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Ein Inhaltsblock einer Nachricht.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Thinking {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<ContentBlock>,
        #[serde(default)]
        is_error: bool,
    },
    Image {
        source: ImageSource,
    },
}

impl ContentBlock {
    /// Bequemer Text-Block.
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock::Text { text: text.into() }
    }
}

/// Bildquelle für `ContentBlock::Image`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ImageSource {
    Base64 { media_type: String, data: String },
    Url { url: String },
}

/// Eine Nachricht im Gesprächsverlauf.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

impl Message {
    /// Nachricht des Nutzers aus reinem Text.
    pub fn user_text(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: vec![ContentBlock::text(text)],
            usage: None,
        }
    }

    /// Assistant-Nachricht aus fertigen Blöcken.
    pub fn assistant(content: Vec<ContentBlock>) -> Self {
        Message {
            role: Role::Assistant,
            content,
            usage: None,
        }
    }

    /// Liefert Referenzen auf alle `ToolUse`-Blöcke dieser Nachricht.
    pub fn tool_uses(&self) -> Vec<&ContentBlock> {
        self.content
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
            .collect()
    }
}

/// Token-Verbrauch eines Provider-Aufrufs (füllt das Kontext-Budget).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_text_builds_single_text_block() {
        let m = Message::user_text("hallo");
        assert_eq!(m.role, Role::User);
        assert_eq!(m.content.len(), 1);
        assert!(matches!(&m.content[0], ContentBlock::Text { text } if text == "hallo"));
    }

    #[test]
    fn tool_uses_filters_blocks() {
        let m = Message::assistant(vec![
            ContentBlock::text("ok"),
            ContentBlock::ToolUse {
                id: "t1".into(),
                name: "bash".into(),
                input: serde_json::json!({"command": "ls"}),
            },
        ]);
        assert_eq!(m.tool_uses().len(), 1);
    }

    #[test]
    fn content_block_serializes_with_type_tag() {
        let v = serde_json::to_value(ContentBlock::text("hi")).unwrap();
        assert_eq!(v["type"], "text");
        assert_eq!(v["text"], "hi");
    }
}
