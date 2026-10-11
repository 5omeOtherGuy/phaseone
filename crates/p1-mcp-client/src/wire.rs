//! MCP result shapes: `tools/list`, `tools/call`, `resources/list` and `resources/read`
//! answers turned into the port's types.

use p1_contracts::mcp::{ContentBlock, McpError, RemoteResource, RemoteTool};
use serde_json::Value;

/// One page of a listing: the items under `key` and the next cursor.
pub(crate) fn page(result: &Value, key: &str) -> Result<(Vec<Value>, Option<String>), McpError> {
    let items = result
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| McpError::Protocol(format!("a listing carries no `{key}` array")))?;
    let next = match result.get("nextCursor") {
        None | Some(Value::Null) => None,
        Some(Value::String(cursor)) => Some(cursor.clone()),
        Some(_) => return Err(McpError::Protocol("`nextCursor` is not a string".into())),
    };
    Ok((items, next))
}

pub(crate) fn remote_tool(tool: &Value) -> Result<RemoteTool, McpError> {
    let name = string(tool, "name")
        .ok_or_else(|| McpError::Protocol("a listed tool has no name".into()))?;
    let input_schema = match tool.get("inputSchema") {
        Some(schema @ Value::Object(_)) => schema.clone(),
        _ => {
            return Err(McpError::Protocol(format!(
                "tool `{name}` has no input schema object"
            )));
        }
    };
    Ok(RemoteTool {
        description: string(tool, "description").unwrap_or_default(),
        input_schema,
        read_only: tool
            .pointer("/annotations/readOnlyHint")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        name,
    })
}

pub(crate) fn remote_resource(resource: &Value) -> Result<RemoteResource, McpError> {
    let uri = string(resource, "uri")
        .ok_or_else(|| McpError::Protocol("a listed resource has no uri".into()))?;
    Ok(RemoteResource {
        name: string(resource, "name").unwrap_or_else(|| uri.clone()),
        description: string(resource, "description"),
        mime_type: string(resource, "mimeType"),
        uri,
    })
}

/// A `tools/call` answer: its content, or [`McpError::ToolFailed`] when the server set
/// `isError`. A result with no content blocks but `structuredContent` shows that as JSON text.
pub(crate) fn call_result(result: &Value) -> Result<Vec<ContentBlock>, McpError> {
    let mut content: Vec<ContentBlock> = result
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| blocks.iter().map(content_block).collect())
        .unwrap_or_default();
    if content.is_empty()
        && let Some(structured) = result.get("structuredContent")
    {
        content.push(ContentBlock::Text {
            text: structured.to_string(),
        });
    }
    if result.get("isError").and_then(Value::as_bool) == Some(true) {
        return Err(McpError::ToolFailed { content });
    }
    Ok(content)
}

/// A `resources/read` answer.
pub(crate) fn resource_contents(result: &Value) -> Result<Vec<ContentBlock>, McpError> {
    let contents = result
        .get("contents")
        .and_then(Value::as_array)
        .ok_or_else(|| McpError::Protocol("a resource read carries no `contents`".into()))?;
    Ok(contents.iter().map(embedded_resource).collect())
}

fn content_block(block: &Value) -> ContentBlock {
    let kind = string(block, "type").unwrap_or_default();
    match kind.as_str() {
        "text" => ContentBlock::Text {
            text: string(block, "text").unwrap_or_default(),
        },
        "image" => ContentBlock::Image {
            mime_type: string(block, "mimeType").unwrap_or_default(),
            data: string(block, "data").unwrap_or_default(),
        },
        "audio" => ContentBlock::Audio {
            mime_type: string(block, "mimeType").unwrap_or_default(),
            data: string(block, "data").unwrap_or_default(),
        },
        "resource" => block
            .get("resource")
            .map(embedded_resource)
            .unwrap_or(ContentBlock::Unknown { kind }),
        "resource_link" => ContentBlock::ResourceLink {
            uri: string(block, "uri").unwrap_or_default(),
            name: string(block, "name"),
            mime_type: string(block, "mimeType"),
        },
        _ => ContentBlock::Unknown { kind },
    }
}

fn embedded_resource(resource: &Value) -> ContentBlock {
    ContentBlock::Resource {
        uri: string(resource, "uri").unwrap_or_default(),
        mime_type: string(resource, "mimeType"),
        text: string(resource, "text"),
        blob: string(resource, "blob"),
    }
}

fn string(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_read_only_hint_is_read_and_absent_means_not_read_only() {
        let hinted = remote_tool(&json!({
            "name": "lookup", "inputSchema": {"type": "object"},
            "annotations": {"readOnlyHint": true}
        }))
        .unwrap();
        assert!(hinted.read_only);
        let plain =
            remote_tool(&json!({"name": "run", "inputSchema": {"type": "object"}})).unwrap();
        assert!(!plain.read_only);
        assert_eq!(plain.description, "");
    }

    #[test]
    fn a_tool_without_a_schema_object_is_a_protocol_error() {
        assert!(matches!(
            remote_tool(&json!({"name": "x"})),
            Err(McpError::Protocol(_))
        ));
    }

    #[test]
    fn every_content_kind_maps_and_is_error_fails_the_call() {
        let result = json!({
            "content": [
                {"type": "text", "text": "hi"},
                {"type": "image", "mimeType": "image/png", "data": "AAAA"},
                {"type": "resource", "resource": {"uri": "file:///a", "text": "body"}},
                {"type": "resource_link", "uri": "file:///b", "name": "b"},
                {"type": "hologram"}
            ]
        });
        let blocks = call_result(&result).unwrap();
        assert_eq!(blocks.len(), 5);
        assert_eq!(
            blocks[4],
            ContentBlock::Unknown {
                kind: "hologram".into()
            }
        );
        let failed =
            call_result(&json!({"content": [{"type": "text", "text": "no"}], "isError": true}));
        assert!(matches!(failed, Err(McpError::ToolFailed { content }) if content.len() == 1));
    }

    #[test]
    fn structured_content_shows_when_there_is_no_content() {
        let blocks = call_result(&json!({"content": [], "structuredContent": {"n": 1}})).unwrap();
        assert_eq!(
            blocks,
            vec![ContentBlock::Text {
                text: r#"{"n":1}"#.into()
            }]
        );
    }
}
