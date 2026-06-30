use std::collections::HashMap;

use pacto_bot_api::errors::DaemonError;

/// A value that can be substituted into a template.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    String(String),
    Bool(bool),
    List(Vec<Value>),
}

impl Value {
    /// Returns true for non-empty strings, true booleans, and non-empty lists.
    pub fn is_truthy(&self) -> bool {
        match self {
            Value::String(s) => !s.is_empty(),
            Value::Bool(b) => *b,
            Value::List(items) => !items.is_empty(),
        }
    }

    /// Returns the string contents if this value is a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Value::String(value.to_string())
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Value::String(value)
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Value::Bool(value)
    }
}

impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(value: Vec<T>) -> Self {
        Value::List(value.into_iter().map(Into::into).collect())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TagKind {
    Var,
    Block,
}

/// Lightweight template substitution engine.
///
/// Supports:
/// * `{{key}}` scalar replacement
/// * `{% if key %}...{% endif %}` conditional blocks
/// * `{% for item in items %}...{% endfor %}` iteration over list values
#[derive(Debug)]
pub struct Template {
    nodes: Result<Vec<Node>, String>,
}

#[derive(Debug)]
enum Node {
    Text(String),
    Var(String),
    If {
        key: String,
        body: Vec<Node>,
    },
    For {
        var: String,
        list: String,
        body: Vec<Node>,
    },
}

impl Template {
    /// Parse a raw template string into an AST.
    pub fn new(raw: &str) -> Self {
        Self {
            nodes: Self::parse(raw).map_err(|err| err.to_string()),
        }
    }

    /// Render the template using the supplied context.
    pub fn render(&self, ctx: &HashMap<String, Value>) -> Result<String, DaemonError> {
        let nodes = self
            .nodes
            .as_ref()
            .map_err(|msg| DaemonError::Config(msg.clone()))?;
        Self::render_nodes(nodes, ctx)
    }

    fn parse(raw: &str) -> Result<Vec<Node>, DaemonError> {
        let mut nodes = Vec::new();
        let mut rest = raw;

        while let Some((start, kind)) = Self::find_next_tag(rest) {
            if start > 0 {
                nodes.push(Node::Text(rest[..start].to_string()));
            }

            match kind {
                TagKind::Var => {
                    let content = &rest[start + 2..];
                    let end = content.find("}}").ok_or_else(|| {
                        DaemonError::Config("unclosed {{ variable tag".to_string())
                    })?;
                    let key = content[..end].trim();
                    if key.is_empty() {
                        return Err(DaemonError::Config("empty variable tag {{}}".to_string()));
                    }
                    if key.split_whitespace().count() != 1 {
                        return Err(DaemonError::Config(format!(
                            "variable tag must contain a single key, got '{{{{{}}}}}'",
                            key
                        )));
                    }
                    nodes.push(Node::Var(key.to_string()));
                    rest = &content[end + 2..];
                }
                TagKind::Block => {
                    let content = &rest[start + 2..];
                    let end = content
                        .find("%}")
                        .ok_or_else(|| DaemonError::Config("unclosed {% block tag".to_string()))?;
                    let inner = content[..end].trim();
                    let mut tokens = inner.split_whitespace();
                    let keyword = tokens.next().unwrap_or("");

                    match keyword {
                        "if" => {
                            let key = tokens
                                .next()
                                .ok_or_else(|| {
                                    DaemonError::Config(format!(
                                        "missing key in if tag: '{{% {inner} %}}'"
                                    ))
                                })?
                                .to_string();
                            if tokens.next().is_some() {
                                return Err(DaemonError::Config(format!(
                                    "if tag takes exactly one key: '{{% {inner} %}}'"
                                )));
                            }
                            let body_start = end + 2;
                            let (match_start, match_end) =
                                Self::find_block_end(&content[body_start..], "if", "endif")?;
                            let body = Self::parse(&content[body_start..body_start + match_start])?;
                            nodes.push(Node::If { key, body });
                            rest = &content[body_start + match_end..];
                        }
                        "for" => {
                            let var = tokens
                                .next()
                                .ok_or_else(|| {
                                    DaemonError::Config(format!(
                                        "missing loop variable in for tag: '{{% {inner} %}}'"
                                    ))
                                })?
                                .to_string();
                            let in_keyword = tokens.next().ok_or_else(|| {
                                DaemonError::Config(format!(
                                    "missing 'in' in for tag: '{{% {inner} %}}'"
                                ))
                            })?;
                            if in_keyword != "in" {
                                return Err(DaemonError::Config(format!(
                                    "for tag must use 'in': '{{% {inner} %}}'"
                                )));
                            }
                            let list = tokens
                                .next()
                                .ok_or_else(|| {
                                    DaemonError::Config(format!(
                                        "missing list in for tag: '{{% {inner} %}}'"
                                    ))
                                })?
                                .to_string();
                            if tokens.next().is_some() {
                                return Err(DaemonError::Config(format!(
                                    "for tag takes 'for var in list': '{{% {inner} %}}'"
                                )));
                            }
                            let body_start = end + 2;
                            let (match_start, match_end) =
                                Self::find_block_end(&content[body_start..], "for", "endfor")?;
                            let body = Self::parse(&content[body_start..body_start + match_start])?;
                            nodes.push(Node::For { var, list, body });
                            rest = &content[body_start + match_end..];
                        }
                        _ => {
                            return Err(DaemonError::Config(format!(
                                "unknown template tag: '{{% {inner} %}}'"
                            )));
                        }
                    }
                }
            }
        }

        if !rest.is_empty() {
            nodes.push(Node::Text(rest.to_string()));
        }

        Ok(nodes)
    }

    fn find_next_tag(s: &str) -> Option<(usize, TagKind)> {
        let var = s.find("{{").map(|idx| (idx, TagKind::Var));
        let block = s.find("{%").map(|idx| (idx, TagKind::Block));

        match (var, block) {
            (Some(v), Some(b)) => {
                if v.0 <= b.0 {
                    Some(v)
                } else {
                    Some(b)
                }
            }
            (Some(v), None) => Some(v),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    fn find_block_end(
        src: &str,
        block_keyword: &str,
        end_keyword: &str,
    ) -> Result<(usize, usize), DaemonError> {
        let mut pos = 0;
        let mut depth = 1;

        loop {
            let tag_start = src[pos..]
                .find("{%")
                .ok_or_else(|| DaemonError::Config(format!("unclosed {block_keyword} block")))?;
            let tag_start_abs = pos + tag_start;
            let tag_end = src[tag_start_abs..].find("%}").ok_or_else(|| {
                DaemonError::Config(format!("unclosed tag inside {block_keyword} block"))
            })?;
            let tag_end_abs = tag_start_abs + tag_end + 2;
            let inner = src[tag_start_abs + 2..tag_end_abs - 2].trim();
            let first = inner.split_whitespace().next().unwrap_or("");

            if first == block_keyword {
                depth += 1;
            } else if first == end_keyword {
                depth -= 1;
                if depth == 0 {
                    return Ok((tag_start_abs, tag_end_abs));
                }
            }
            pos = tag_end_abs;
        }
    }

    fn render_nodes(nodes: &[Node], ctx: &HashMap<String, Value>) -> Result<String, DaemonError> {
        let mut out = String::new();

        for node in nodes {
            match node {
                Node::Text(text) => out.push_str(text),
                Node::Var(key) => {
                    let value = ctx.get(key).ok_or_else(|| {
                        DaemonError::Config(format!("missing template key: {key}"))
                    })?;
                    match value {
                        Value::String(s) => out.push_str(s.as_str()),
                        Value::Bool(b) => out.push_str(&b.to_string()),
                        Value::List(_) => {
                            return Err(DaemonError::Config(format!(
                                "template key '{key}' is a list and cannot be rendered as a scalar"
                            )));
                        }
                    }
                }
                Node::If { key, body } => {
                    if ctx.get(key).map(Value::is_truthy).unwrap_or(false) {
                        out.push_str(&Self::render_nodes(body, ctx)?);
                    }
                }
                Node::For { var, list, body } => match ctx.get(list) {
                    Some(Value::List(items)) => {
                        for item in items {
                            let mut child_ctx = ctx.clone();
                            child_ctx.insert(var.clone(), item.clone());
                            out.push_str(&Self::render_nodes(body, &child_ctx)?);
                        }
                    }
                    Some(_) => {
                        return Err(DaemonError::Config(format!(
                            "template key '{list}' is not a list"
                        )));
                    }
                    None => {}
                },
            }
        }

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::expect_used)]
    #![allow(clippy::panic)]

    use super::*;

    fn render(raw: &str, ctx: HashMap<String, Value>) -> String {
        Template::new(raw).render(&ctx).unwrap()
    }

    #[test]
    fn replaces_string_placeholder() {
        let mut ctx = HashMap::new();
        ctx.insert("bot_id".to_string(), Value::from("echo-bot"));
        assert_eq!(render("id={{bot_id}}", ctx), "id=echo-bot");
    }

    #[test]
    fn replaces_multiple_placeholders() {
        let mut ctx = HashMap::new();
        ctx.insert("bot_id".to_string(), Value::from("echo-bot"));
        ctx.insert("language".to_string(), Value::from("python"));
        assert_eq!(
            render("{{bot_id}} uses {{language}}", ctx),
            "echo-bot uses python"
        );
    }

    #[test]
    fn renders_bool_placeholder() {
        let mut ctx = HashMap::new();
        ctx.insert("enabled".to_string(), Value::from(true));
        assert_eq!(render("enabled={{enabled}}", ctx), "enabled=true");
    }

    #[test]
    fn leaves_literal_text_untouched() {
        let ctx = HashMap::new();
        assert_eq!(
            render("no placeholders here {not a tag}", ctx),
            "no placeholders here {not a tag}"
        );
    }

    #[test]
    fn conditional_block_renders_when_truthy() {
        let mut ctx = HashMap::new();
        ctx.insert("with_tests".to_string(), Value::from(true));
        assert_eq!(
            render("{% if with_tests %}tests{% endif %}done", ctx),
            "testsdone"
        );
    }

    #[test]
    fn conditional_block_skips_when_falsy() {
        let mut ctx = HashMap::new();
        ctx.insert("with_tests".to_string(), Value::from(false));
        assert_eq!(
            render("{% if with_tests %}tests{% endif %}done", ctx),
            "done"
        );
    }

    #[test]
    fn conditional_block_skips_when_missing() {
        let ctx = HashMap::new();
        assert_eq!(
            render("{% if with_tests %}tests{% endif %}done", ctx),
            "done"
        );
    }

    #[test]
    fn conditional_block_renders_for_non_empty_string() {
        let mut ctx = HashMap::new();
        ctx.insert("name".to_string(), Value::from("echo-bot"));
        assert_eq!(render("{% if name %}hello{% endif %}", ctx), "hello");
    }

    #[test]
    fn conditional_block_skips_for_empty_string() {
        let mut ctx = HashMap::new();
        ctx.insert("name".to_string(), Value::from(""));
        assert_eq!(render("{% if name %}hello{% endif %}", ctx), "");
    }

    #[test]
    fn loop_renders_body_for_each_item() {
        let ctx = HashMap::from([("commands".to_string(), Value::from(vec!["echo", "help"]))]);
        assert_eq!(
            render("{% for cmd in commands %}{{cmd}} {% endfor %}", ctx),
            "echo help "
        );
    }

    #[test]
    fn loop_over_empty_list_renders_nothing() {
        let ctx = HashMap::from([("commands".to_string(), Value::List(Vec::new()))]);
        assert_eq!(
            render(
                "before{% for cmd in commands %}{{cmd}}{% endfor %}after",
                ctx
            ),
            "beforeafter"
        );
    }

    #[test]
    fn loop_over_missing_list_renders_nothing() {
        let ctx = HashMap::new();
        assert_eq!(
            render(
                "before{% for cmd in commands %}{{cmd}}{% endfor %}after",
                ctx
            ),
            "beforeafter"
        );
    }

    #[test]
    fn loop_body_can_reference_outer_context() {
        let mut ctx = HashMap::new();
        ctx.insert("bot_id".to_string(), Value::from("echo-bot"));
        ctx.insert("commands".to_string(), Value::from(vec!["echo"]));
        assert_eq!(
            render(
                "{% for cmd in commands %}{{bot_id}}:{{cmd}}{% endfor %}",
                ctx
            ),
            "echo-bot:echo"
        );
    }

    #[test]
    fn nested_conditionals_work() {
        let mut ctx = HashMap::new();
        ctx.insert("outer".to_string(), Value::from(true));
        ctx.insert("inner".to_string(), Value::from(false));
        assert_eq!(
            render(
                "{% if outer %}{% if inner %}yes{% endif %}no{% endif %}",
                ctx
            ),
            "no"
        );
    }

    #[test]
    fn loop_inside_conditional_works() {
        let mut ctx = HashMap::new();
        ctx.insert("enabled".to_string(), Value::from(true));
        ctx.insert("commands".to_string(), Value::from(vec!["a", "b"]));
        assert_eq!(
            render(
                "{% if enabled %}{% for cmd in commands %}{{cmd}}{% endfor %}{% endif %}",
                ctx
            ),
            "ab"
        );
    }

    #[test]
    fn missing_scalar_key_returns_error() {
        let ctx = HashMap::new();
        let err = Template::new("{{missing}}").render(&ctx).unwrap_err();
        assert!(err.to_string().contains("missing template key: missing"));
    }

    #[test]
    fn list_cannot_be_rendered_as_scalar() {
        let ctx = HashMap::from([("items".to_string(), Value::from(vec!["a", "b"]))]);
        let err = Template::new("{{items}}").render(&ctx).unwrap_err();
        assert!(err.to_string().contains("is a list"));
    }

    #[test]
    fn unclosed_variable_tag_returns_error() {
        let err = Template::new("{{unclosed")
            .render(&HashMap::new())
            .unwrap_err();
        assert!(err.to_string().contains("unclosed {{ variable tag"));
    }

    #[test]
    fn unclosed_if_block_returns_error() {
        let mut ctx = HashMap::new();
        ctx.insert("ok".to_string(), Value::from(true));
        let err = Template::new("{% if ok %}never closed")
            .render(&ctx)
            .unwrap_err();
        assert!(err.to_string().contains("unclosed if block"));
    }

    #[test]
    fn unknown_tag_returns_error() {
        let err = Template::new("{% unknown %}")
            .render(&HashMap::new())
            .unwrap_err();
        assert!(err.to_string().contains("unknown template tag"));
    }
}
