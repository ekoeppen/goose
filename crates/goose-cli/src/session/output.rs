use anstream::println;
use bat::WrappingMode;
use console::{measure_text_width, style, Color, Term};
use goose::config::Config;
use goose::conversation::message::{
    ActionRequiredData, Message, MessageContent, ToolRequest, ToolResponse,
};
use goose::permission::Permission;
use goose::providers::canonical::maybe_get_canonical_model;
#[cfg(target_os = "windows")]
use goose::subprocess::SubprocessExt;
use goose::utils::safe_truncate;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use rmcp::model::{CallToolRequestParams, JsonObject, PromptArgument};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Error, IsTerminal, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub const DEFAULT_MIN_PRIORITY: f32 = 0.0;
pub const DEFAULT_CLI_LIGHT_THEME: &str = "GitHub";
pub const DEFAULT_CLI_DARK_THEME: &str = "zenburn";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ContentType {
    #[default]
    Empty,
    Text,
    ToolCall,
    ToolResponse,
    System,
    Prompt,
    Header,
    Error,
}

// Re-export theme for use in main
#[derive(Clone, Copy)]
pub enum Theme {
    Light,
    Dark,
    Ansi,
}

impl Theme {
    fn as_str(&self) -> String {
        match self {
            Theme::Light => Config::global()
                .get_param::<String>("GOOSE_CLI_LIGHT_THEME")
                .unwrap_or(DEFAULT_CLI_LIGHT_THEME.to_string()),
            Theme::Dark => Config::global()
                .get_param::<String>("GOOSE_CLI_DARK_THEME")
                .unwrap_or(DEFAULT_CLI_DARK_THEME.to_string()),
            Theme::Ansi => "base16".to_string(),
        }
    }

    fn from_config_str(val: &str) -> Self {
        if val.eq_ignore_ascii_case("light") {
            Theme::Light
        } else if val.eq_ignore_ascii_case("ansi") {
            Theme::Ansi
        } else {
            Theme::Dark
        }
    }

    fn as_config_string(&self) -> String {
        match self {
            Theme::Light => "light".to_string(),
            Theme::Dark => "dark".to_string(),
            Theme::Ansi => "ansi".to_string(),
        }
    }
}

// Simple wrapper around spinner to manage its state
#[derive(Default)]
pub struct ThinkingIndicator {
    spinner: Option<cliclack::ProgressBar>,
}

pub const INDENT: &str = "  ";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PromptInfo {
    pub name: String,
    pub description: Option<String>,
    pub arguments: Option<Vec<PromptArgument>>,
    pub extension: Option<String>,
}

pub struct SessionOutput {
    pub theme: Theme,
    pub show_full_tool_output: bool,
    pub last_rendered: ContentType,
    pub thinking: ThinkingIndicator,
    pub quiet: bool,
    pub text_messages: String,
}

impl ThinkingIndicator {
    pub fn show(&mut self) {
        if self.spinner.is_none() {
            self.spinner = Some(cliclack::progress_bar(0));
            if let Some(spinner) = self.spinner.as_mut() {
                spinner.start("Thinking...");
            }
        }
    }

    pub fn hide(&mut self) {
        if let Some(spinner) = self.spinner.take() {
            spinner.stop("Thinking...");
        }
    }

    pub fn is_shown(&self) -> bool {
        self.spinner.is_some()
    }
}

impl Drop for SessionOutput {
    fn drop(&mut self) {
        // Ensure thinking indicator is hidden when SessionOutput goes out of scope
        self.thinking.hide();
    }
}

impl SessionOutput {
    pub fn new() -> Self {
        let theme = Config::global()
            .get_param::<String>("GOOSE_CLI_THEME")
            .ok()
            .map(|val| Theme::from_config_str(&val))
            .unwrap_or_else(|| {
                std::env::var("GOOSE_CLI_THEME")
                    .ok()
                    .map(|val| Theme::from_config_str(&val))
                    .unwrap_or(Theme::Ansi)
            });

        let show_full_tool_output = Config::global()
            .get_param::<bool>("GOOSE_CLI_SHOW_FULL_TOOL_OUTPUT")
            .ok()
            .unwrap_or(false);

        Self {
            theme,
            show_full_tool_output,
            last_rendered: ContentType::Empty,
            thinking: ThinkingIndicator::default(),
            quiet: false,
            text_messages: String::new(),
        }
    }

    pub fn print_markdown(&self, text: &str) {
        if self.quiet {
            return;
        }

        let theme_name = self.theme.as_str();

        // If we can't find the theme, or if it's not a terminal, just print plain text
        if !std::io::stdout().is_terminal() {
            println!("{}", text);
            return;
        }

        // Try to use bat for syntax highlighting
        let mut bat_printer = bat::PrettyPrinter::new();
        bat_printer
            .input_from_bytes(text.as_bytes())
            .language("markdown")
            .theme(&theme_name)
            .wrapping_mode(WrappingMode::Character)
            .true_color(true);

        if let Err(e) = bat_printer.print() {
            tracing::error!("Failed to print markdown with bat: {}", e);
            println!("{}", text);
        }
    }

    pub fn print_tool_header(&self, call: &CallToolRequestParams) {
        if self.quiet {
            return;
        }
        println!("  {} {}", style("▸").dim(), style(&call.name).dim());
    }

    pub fn format_subagent_tool_call_message(&self, subagent_id: &str, tool_name: &str) -> String {
        let short_id = subagent_id.rsplit('_').next().unwrap_or(subagent_id);
        format!("[subagent:{}] {}", short_id, tool_name)
    }

    pub fn with_quiet(mut self, quiet: bool) -> Self {
        self.quiet = quiet;
        self
    }

    pub fn set_theme(&mut self, theme: Theme) {
        if let Err(e) = Config::global().set_param("GOOSE_CLI_THEME", theme.as_config_string()) {
            eprintln!("Failed to save theme setting to config: {}", e);
        }
        self.theme = theme;
    }

    pub fn get_theme(&self) -> Theme {
        self.theme
    }

    pub fn toggle_full_tool_output(&mut self) -> bool {
        self.show_full_tool_output = !self.show_full_tool_output;
        if let Err(e) = Config::global().set_param(
            "GOOSE_CLI_SHOW_FULL_TOOL_OUTPUT",
            self.show_full_tool_output,
        ) {
            eprintln!("Failed to save full tool output setting to config: {}", e);
        }
        self.show_full_tool_output
    }

    pub fn get_show_full_tool_output(&self) -> bool {
        self.show_full_tool_output
    }

    pub fn show_thinking(&mut self) {
        if std::io::stdout().is_terminal() {
            self.thinking.show();
        }
    }

    pub fn hide_thinking(&mut self) {
        if std::io::stdout().is_terminal() {
            self.thinking.hide();
        }
    }

    pub fn prompt_tool_confirmation(
        &self,
        security_prompt: &Option<String>,
    ) -> anyhow::Result<Permission> {
        let prompt = if let Some(security_message) = security_prompt {
            println!("\n{}", security_message);
            "Do you allow this tool call?".to_string()
        } else {
            "Goose would like to call the above tool, do you allow?".to_string()
        };

        let confirmed = cliclack::confirm(prompt).initial_value(false).interact()?;

        Ok(if confirmed {
            Permission::AllowOnce
        } else {
            Permission::Cancel
        })
    }

    pub fn is_showing_thinking(&self) -> bool {
        self.thinking.is_shown()
    }

    pub fn set_thinking_message(&mut self, s: &str) {
        if std::io::stdout().is_terminal() {
            if let Some(spinner) = self.thinking.spinner.as_mut() {
                spinner.set_message(s);
            }
        }
    }

    fn handle_spacing(&mut self, next: ContentType) {
        match (self.last_rendered, next) {
            (ContentType::Empty, _) => {} // Start of session, no extra newline
            (ContentType::Header, _) | (_, ContentType::Header) => {
                println!();
            }
            (ContentType::ToolCall, ContentType::ToolResponse) => {
                // Keep them tight together
            }
            _ => {
                println!();
            }
        }
        self.last_rendered = next;
    }

    pub fn render_message(&mut self, message: &Message, debug: bool) {
        if self.quiet {
            return;
        }
        for content in &message.content {
            match content {
                MessageContent::ActionRequired(action) => match &action.data {
                    ActionRequiredData::ToolConfirmation { .. } => {
                        // Handled by prompt_tool_confirmation in mod.rs
                    }
                    ActionRequiredData::Elicitation { .. } => {
                        // Handled by collect_elicitation_input in mod.rs
                    }
                    ActionRequiredData::ElicitationResponse { .. } => {
                        // Internal state
                    }
                },
                MessageContent::Text(text) => self.buffer_text_message(&text.text),
                MessageContent::ToolRequest(req) => {
                    self.handle_spacing(ContentType::ToolCall);
                    self.render_tool_request(req, debug);
                }
                MessageContent::ToolResponse(resp) => {
                    self.handle_spacing(ContentType::ToolResponse);
                    self.render_tool_response(resp, debug);
                }
                MessageContent::Image(image) => {
                    self.handle_spacing(ContentType::Text);
                    println!("Image: [data: {}, type: {}]", image.data, image.mime_type);
                }
                MessageContent::Thinking(thinking) => {
                    if std::env::var("GOOSE_CLI_SHOW_THINKING").is_ok()
                        && std::io::stdout().is_terminal()
                    {
                        self.handle_spacing(ContentType::System);
                        println!("{}", style("Thinking:").dim().italic());
                        self.print_markdown(&thinking.thinking);
                    }
                }
                MessageContent::RedactedThinking(_) => {
                    self.handle_spacing(ContentType::System);
                    println!("{}", style("Thinking:").dim().italic());
                    self.print_markdown("Thinking was redacted");
                }
                MessageContent::SystemNotification(notification) => {
                    use goose::conversation::message::SystemNotificationType;

                    match notification.notification_type {
                        SystemNotificationType::ThinkingMessage => {
                            self.show_thinking();
                            self.set_thinking_message(&notification.msg);
                        }
                        SystemNotificationType::InlineMessage => {
                            self.hide_thinking();
                            self.handle_spacing(ContentType::System);
                            println!("{}", style(&notification.msg).yellow());
                        }
                    }
                }
                _ => {
                    self.handle_spacing(ContentType::Error);
                    println!("WARNING: Message content type could not be rendered");
                }
            }
        }

        let _ = std::io::stdout().flush();
    }

    fn buffer_text_message(&mut self, message: &str) {
        self.text_messages.push_str(message);
    }

    pub fn finish(&mut self) {
        if self.quiet {
            return;
        }
        self.handle_spacing(ContentType::Text);
        self.print_markdown(&self.text_messages);
        self.text_messages.clear();
    }

    pub fn render_text(&mut self, text: &str, color: Option<Color>, dim: bool) {
        if self.quiet {
            return;
        }
        self.handle_spacing(ContentType::Text);
        self.render_text_raw(text, color, dim);
    }

    pub fn render_text_raw(&mut self, text: &str, color: Option<Color>, dim: bool) {
        if self.quiet {
            return;
        }
        if !std::io::stdout().is_terminal() {
            println!("{}", text);
            return;
        }
        let mut styled_text = style(text);
        if dim {
            styled_text = styled_text.dim();
        }
        if let Some(color) = color {
            styled_text = styled_text.fg(color);
        } else {
            styled_text = styled_text.green();
        }
        print!("{}", styled_text);
        let _ = std::io::stdout().flush();
    }

    pub fn render_error(&mut self, message: &str) {
        self.handle_spacing(ContentType::Error);
        println!("  {} {}", style("error:").red().bold(), message);
    }

    pub fn render_header(&mut self, text: &str) {
        if self.quiet {
            return;
        }
        self.handle_spacing(ContentType::Header);
        println!("{}", style(text).bold());
    }

    pub fn render_enter_plan_mode(&mut self) {
        if self.quiet {
            return;
        }
        self.handle_spacing(ContentType::System);
        println!(
            "{} {}",
            style("Entering plan mode.").green().bold(),
            style("You can provide instructions to create a plan and then act on it. To exit early, type /endplan")
                .green()
                .dim()
        );
    }

    pub fn render_act_on_plan(&mut self) {
        if self.quiet {
            return;
        }
        self.handle_spacing(ContentType::System);
        println!(
            "{}",
            style("Exiting plan mode and acting on the above plan")
                .green()
                .bold(),
        );
    }

    pub fn render_exit_plan_mode(&mut self) {
        if self.quiet {
            return;
        }
        self.handle_spacing(ContentType::System);
        println!("{}", style("Exiting plan mode.").green().bold());
    }

    pub fn goose_mode_message(&mut self, text: &str) {
        if self.quiet {
            return;
        }
        self.handle_spacing(ContentType::System);
        println!("{}", style(text).yellow());
    }

    fn render_tool_request(&mut self, req: &ToolRequest, debug: bool) {
        match &req.tool_call {
            Ok(call) => match call.name.to_string().as_str() {
                "developer__text_editor" => self.render_text_editor_request(call, debug),
                "developer__shell" => self.render_shell_request(call, debug),
                "execute" | "execute_code" => self.render_execute_code_request(call, debug),
                "delegate" => self.render_delegate_request(call, debug),
                "subagent" => self.render_delegate_request(call, debug),
                "todo__write" => self.render_todo_request(call, debug),
                _ => self.render_default_request(call, debug),
            },
            Err(e) => self.print_markdown(&e.to_string()),
        }
    }

    fn render_tool_response(&mut self, resp: &ToolResponse, debug: bool) {
        match &resp.tool_result {
            Ok(result) => {
                for content in &result.content {
                    if let Some(audience) = content.audience() {
                        if !audience.contains(&rmcp::model::Role::User) {
                            continue;
                        }
                    }

                    let min_priority = Config::global()
                        .get_param::<f32>("GOOSE_CLI_MIN_PRIORITY")
                        .ok()
                        .unwrap_or(DEFAULT_MIN_PRIORITY);

                    if content
                        .priority()
                        .is_some_and(|priority| priority < min_priority)
                        || (content.priority().is_none() && !debug)
                    {
                        continue;
                    }

                    if debug {
                        println!("{:#?}", content);
                    } else if let Some(text) = content.as_text() {
                        self.print_markdown(&text.text);
                    }
                }
            }
            Err(e) => self.print_markdown(&e.to_string()),
        }
    }

    pub fn render_prompts(&mut self, prompts: &HashMap<String, Vec<String>>) {
        if self.quiet {
            return;
        }
        self.handle_spacing(ContentType::Prompt);
        for (extension, prompts) in prompts {
            println!(" {}", style(extension).green());
            for prompt in prompts {
                println!("  - {}", style(prompt).cyan());
            }
        }
    }

    pub fn render_prompt_info(&mut self, info: &PromptInfo) {
        if self.quiet {
            return;
        }
        self.handle_spacing(ContentType::Prompt);
        if let Some(ext) = &info.extension {
            println!(" {}: {}", style("Extension").green(), ext);
        }
        println!(" Prompt: {}", style(&info.name).cyan().bold());
        if let Some(desc) = &info.description {
            println!("\n {}", desc);
        }
        self.render_arguments(info);
    }

    fn render_arguments(&mut self, info: &PromptInfo) {
        if self.quiet {
            return;
        }
        if let Some(args) = &info.arguments {
            println!("\n Arguments:");
            for arg in args {
                let required = arg.required.unwrap_or(false);
                let req_str = if required {
                    style("(required)").red()
                } else {
                    style("(optional)").dim()
                };

                println!(
                    "  {} {} {}",
                    style(&arg.name).yellow(),
                    req_str,
                    arg.description.as_deref().unwrap_or("")
                );
            }
        }
    }

    pub fn render_extension_success(&mut self, name: &str) {
        if self.quiet {
            return;
        }
        self.handle_spacing(ContentType::System);
        println!(
            "  {} extension `{}`",
            style("added").green(),
            style(name).cyan(),
        );
    }

    pub fn render_extension_error(&mut self, name: &str, error: &str) {
        self.handle_spacing(ContentType::Error);
        println!(
            "  {} to add extension {}",
            style("failed").red(),
            style(name).red()
        );
        println!("{}", style(error).dim());
    }

    pub fn render_builtin_success(&mut self, names: &str) {
        if self.quiet {
            return;
        }
        self.handle_spacing(ContentType::System);
        println!(
            "  {} builtin{}: {}",
            style("added").green(),
            if names.contains(',') { "s" } else { "" },
            style(names).cyan()
        );
    }

    pub fn render_builtin_error(&mut self, names: &str, error: &str) {
        self.handle_spacing(ContentType::Error);
        println!(
            "  {} to add builtin{}: {}",
            style("failed").red(),
            if names.contains(',') { "s" } else { "" },
            style(names).red()
        );
        println!("{}", style(error).dim());
    }

    fn render_text_editor_request(&mut self, call: &CallToolRequestParams, debug: bool) {
        self.print_tool_header(call);

        if let Some(args) = &call.arguments {
            if let Some(Value::String(path)) = args.get("path") {
                println!(
                    "    {} {}",
                    style("path").dim(),
                    style(shorten_path(path, debug)).dim()
                );
            }

            let mut other_args = serde_json::Map::new();
            for (k, v) in args {
                if k != "path" {
                    other_args.insert(k.clone(), v.clone());
                }
            }
            if !other_args.is_empty() {
                self.print_params(&Some(other_args), 1, debug);
            }
        }
    }

    fn render_shell_request(&mut self, call: &CallToolRequestParams, debug: bool) {
        self.print_tool_header(call);
        self.print_params(&call.arguments, 1, debug);
    }

    fn render_execute_code_request(&mut self, call: &CallToolRequestParams, debug: bool) {
        let tool_graph = call
            .arguments
            .as_ref()
            .and_then(|args| args.get("tool_graph"))
            .and_then(Value::as_array)
            .filter(|arr| !arr.is_empty());

        let Some(tool_graph) = tool_graph else {
            return self.render_default_request(call, debug);
        };

        let count = tool_graph.len();
        let plural = if count == 1 { "" } else { "s" };
        println!(
            "  {} {} {} tool call{}",
            style("▸").dim(),
            style("execute").dim(),
            style(count).dim(),
            plural,
        );

        for (i, node) in tool_graph.iter().filter_map(Value::as_object).enumerate() {
            let tool = node
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let desc = node
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");
            let deps: Vec<_> = node
                .get("depends_on")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_u64)
                .map(|d| (d + 1).to_string())
                .collect();
            let deps_str = if deps.is_empty() {
                String::new()
            } else {
                format!(" (uses {})", deps.join(", "))
            };
            println!(
                "    {}. {} {}{}",
                style(i + 1).dim(),
                style(tool).dim(),
                style(desc).dim(),
                style(deps_str).dim()
            );
        }

        let code = call
            .arguments
            .as_ref()
            .and_then(|args| args.get("code"))
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty());
        if code.is_some_and(|_| debug) {
            println!("{}", style(code.unwrap_or_default()).green());
        }
    }

    fn render_delegate_request(&mut self, call: &CallToolRequestParams, debug: bool) {
        self.print_tool_header(call);

        if let Some(args) = &call.arguments {
            if let Some(Value::String(source)) = args.get("source") {
                println!("    {} {}", style("source").dim(), style(source).dim());
            }

            if let Some(Value::String(instructions)) = args.get("instructions") {
                let display = if instructions.len() > 100 && !debug {
                    safe_truncate(instructions, 100)
                } else {
                    instructions.clone()
                };
                println!(
                    "    {} {}",
                    style("instructions").dim(),
                    style(display).dim()
                );
            }

            if let Some(Value::Object(params)) = args.get("parameters") {
                println!("    {}:", style("parameters").dim());
                self.print_params(&Some(params.clone()), 2, debug);
            }

            let skip_keys = ["source", "instructions", "parameters"];
            let mut other_args = serde_json::Map::new();
            for (k, v) in args {
                if !skip_keys.contains(&k.as_str()) {
                    other_args.insert(k.clone(), v.clone());
                }
            }
            if !other_args.is_empty() {
                self.print_params(&Some(other_args), 1, debug);
            }
        }
    }

    fn render_todo_request(&mut self, call: &CallToolRequestParams, _debug: bool) {
        self.print_tool_header(call);

        if let Some(args) = &call.arguments {
            if let Some(Value::String(content)) = args.get("content") {
                println!("    {} {}", style("content").dim(), style(content).dim());
            }
        }
    }

    fn render_default_request(&mut self, call: &CallToolRequestParams, debug: bool) {
        self.print_tool_header(call);
        self.print_params(&call.arguments, 1, debug);
    }

    pub fn render_subagent_tool_call(
        &mut self,
        subagent_id: &str,
        tool_name: &str,
        arguments: Option<&JsonObject>,
        debug: bool,
    ) {
        if self.quiet {
            return;
        }
        if tool_name == "code_execution__execute_code" {
            let tool_graph = arguments
                .and_then(|args| args.get("tool_graph"))
                .and_then(Value::as_array)
                .filter(|arr| !arr.is_empty());
            if let Some(tool_graph) = tool_graph {
                return self.render_subagent_tool_graph(subagent_id, tool_graph);
            }
        }
        self.handle_spacing(ContentType::ToolCall);
        let tool_header = format!(
            "  {} {}",
            style("▸").dim(),
            style(self.format_subagent_tool_call_message(subagent_id, tool_name)).dim(),
        );
        println!("{}", tool_header);
        self.print_params(&arguments.cloned(), 1, debug);
    }

    fn render_subagent_tool_graph(&mut self, subagent_id: &str, tool_graph: &[Value]) {
        if self.quiet {
            return;
        }
        self.handle_spacing(ContentType::ToolCall);
        let short_id = subagent_id.rsplit('_').next().unwrap_or(subagent_id);
        let count = tool_graph.len();
        let plural = if count == 1 { "" } else { "s" };
        println!(
            "  {} {} {} {} tool call{}",
            style("▸").dim(),
            style(format!("[subagent:{}]", short_id)).dim(),
            style("execute_code").dim(),
            style(count).dim(),
            plural,
        );

        for (i, node) in tool_graph.iter().filter_map(Value::as_object).enumerate() {
            let tool = node
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let desc = node
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");
            let deps: Vec<_> = node
                .get("depends_on")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_u64)
                .map(|d| (d + 1).to_string())
                .collect();
            let deps_str = if deps.is_empty() {
                String::new()
            } else {
                format!(" (uses {})", deps.join(", "))
            };
            println!(
                "    {}. {} {}{}",
                style(i + 1).dim(),
                style(tool).dim(),
                style(desc).dim(),
                style(deps_str).dim()
            );
        }
    }

    fn print_value_with_prefix(&self, prefix: &str, value: &Value, debug: bool) {
        let prefix_width = measure_text_width(prefix);
        print!("{}", prefix);
        self.print_value(value, debug, prefix_width)
    }

    fn print_value(&self, value: &Value, debug: bool, reserve_width: usize) {
        let max_width = Term::stdout()
            .size_checked()
            .map(|(_h, w)| (w as usize).saturating_sub(reserve_width));
        let show_full = self.show_full_tool_output;
        let formatted = match value {
            Value::String(s) => match (max_width, debug || show_full) {
                (Some(w), false) if s.len() > w => style(safe_truncate(s, w)),
                _ => style(s.to_string()),
            }
            .green(),
            Value::Number(n) => style(n.to_string()).yellow(),
            Value::Bool(b) => style(b.to_string()).yellow(),
            Value::Null => style("null".to_string()).dim(),
            _ => unreachable!(),
        };
        println!("{}", formatted);
    }

    fn print_params(&self, value: &Option<JsonObject>, depth: usize, debug: bool) {
        let indent = INDENT.repeat(depth);

        if let Some(json_object) = value {
            for (key, val) in json_object.iter() {
                match val {
                    Value::Object(obj) => {
                        println!("{}{}:", indent, style(key).dim());
                        self.print_params(&Some(obj.clone()), depth + 1, debug);
                    }
                    Value::Array(arr) => {
                        // Check if all items are simple values (not objects or arrays)
                        let all_simple = arr.iter().all(|item| {
                            matches!(
                                item,
                                Value::String(_) | Value::Number(_) | Value::Bool(_) | Value::Null
                            )
                        });

                        if all_simple {
                            // Render inline for simple arrays, truncation will be handled by print_value if needed
                            let values: Vec<String> = arr
                                .iter()
                                .map(|item| match item {
                                    Value::String(s) => s.clone(),
                                    Value::Number(n) => n.to_string(),
                                    Value::Bool(b) => b.to_string(),
                                    Value::Null => "null".to_string(),
                                    _ => unreachable!(),
                                })
                                .collect();
                            let joined_values = values.join(", ");
                            self.print_value_with_prefix(
                                &format!("{}{}: ", indent, style(key).dim()),
                                &Value::String(joined_values),
                                debug,
                            );
                        } else {
                            // Use the original multi-line format for complex arrays
                            println!("{}{}:", indent, style(key).dim());
                            for item in arr.iter() {
                                if let Value::Object(obj) = item {
                                    println!("{}{}- ", indent, INDENT);
                                    self.print_params(&Some(obj.clone()), depth + 2, debug);
                                } else {
                                    println!("{}{}- {}", indent, INDENT, item);
                                }
                            }
                        }
                    }
                    _ => {
                        self.print_value_with_prefix(
                            &format!("{}{}: ", indent, style(key).dim()),
                            val,
                            debug,
                        );
                    }
                }
            }
        }
    }

    pub fn display_session_info(
        &mut self,
        resume: bool,
        provider: &str,
        model: &str,
        session_id: &Option<String>,
        provider_instance: Option<&Arc<dyn goose::providers::base::Provider>>,
    ) {
        if self.quiet {
            return;
        }
        self.handle_spacing(ContentType::System);
        let status = if resume {
            "resuming"
        } else if session_id.is_none() {
            "ephemeral"
        } else {
            "new session"
        };

        let model_display = if let Some(provider_inst) = provider_instance {
            if let Some(lead_worker) = provider_inst.as_lead_worker() {
                let (lead_model, worker_model) = lead_worker.get_model_info();
                format!("{} → {}", lead_model, worker_model)
            } else {
                model.to_string()
            }
        } else {
            model.to_string()
        };

        println!(
            "  {} {} {} {} {}",
            style("●").green(),
            style(status).dim(),
            style("·").dim(),
            style(provider).dim(),
            style(&model_display).cyan(),
        );

        let cwd_display = std::env::current_dir()
            .ok()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        if let Some(id) = session_id {
            println!(
                "  {} {} {}",
                style(" ").dim(),
                style(id).dim(),
                style(format!("· {}", cwd_display)).dim(),
            );
        } else {
            println!(
                "  {} {}",
                style(" ").dim(),
                style(format!("  {}", cwd_display)).dim(),
            );
        }
    }

    pub fn set_terminal_title(&self) {
        if self.quiet || !std::io::stdout().is_terminal() {
            return;
        }
        let dir_name = std::env::current_dir()
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        // Sanitize: strip control characters (ESC, BEL, etc.) to prevent terminal escape injection
        let sanitized: String = dir_name.chars().filter(|c| !c.is_control()).collect();
        // OSC 0 sets the terminal window/tab title
        print!("\x1b]0;🪿 {}\x07", sanitized);
        let _ = std::io::stdout().flush();
    }

    pub fn display_greeting(&mut self) {
        if self.quiet {
            return;
        }
        self.set_terminal_title();
        self.handle_spacing(ContentType::System);
        println!(
            "{} {}",
            style("🪿 goose").bold(),
            style("ready — type a message to get started").dim()
        );
    }

    pub fn display_context_usage(&self, total_tokens: usize, context_limit: usize) {
        if self.quiet {
            return;
        }
        if context_limit == 0 {
            println!(
                "  {}",
                style("context usage unavailable (context limit is 0)").dim()
            );
            return;
        }

        let percentage =
            (((total_tokens as f64 / context_limit as f64) * 100.0).round() as usize).min(100);

        let bar_width = 20;
        let filled = ((percentage as f64 / 100.0) * bar_width as f64).round() as usize;
        let empty = bar_width - filled.min(bar_width);

        let bar = format!("{}{}", "━".repeat(filled), "╌".repeat(empty));
        let colored_bar = if percentage < 50 {
            style(bar).green().dim()
        } else if percentage < 85 {
            style(bar).yellow()
        } else {
            style(bar).red()
        };

        fn format_tokens(n: usize) -> String {
            if n >= 1_000_000 {
                format!("{:.1}M", n as f64 / 1_000_000.0)
            } else if n >= 1_000 {
                format!("{:.0}k", n as f64 / 1_000.0)
            } else {
                n.to_string()
            }
        }

        println!(
            "  {} {} {}",
            colored_bar,
            style(format!("{}%", percentage)).dim(),
            style(format!(
                "{}/{}",
                format_tokens(total_tokens),
                format_tokens(context_limit)
            ))
            .dim(),
        );
    }

    /// Display cost information, if price data is available.
    pub fn display_cost_usage(
        &self,
        provider: &str,
        model: &str,
        input_tokens: usize,
        output_tokens: usize,
    ) {
        if self.quiet {
            return;
        }
        if let Some(cost) = estimate_cost_usd(provider, model, input_tokens, output_tokens) {
            use console::style;
            eprintln!(
                "Cost: {} USD ({} tokens: in {}, out {})",
                style(format!("${:.4}", cost)).cyan(),
                input_tokens + output_tokens,
                input_tokens,
                output_tokens
            );
        }
    }
}

fn estimate_cost_usd(
    provider: &str,
    model: &str,
    input_tokens: usize,
    output_tokens: usize,
) -> Option<f64> {
    let canonical_model = maybe_get_canonical_model(provider, model)?;

    let input_cost_per_token = canonical_model.cost.input? / 1_000_000.0;
    let output_cost_per_token = canonical_model.cost.output? / 1_000_000.0;

    let input_cost = input_cost_per_token * input_tokens as f64;
    let output_cost = output_cost_per_token * output_tokens as f64;
    Some(input_cost + output_cost)
}

pub fn run_status_hook(status: &str) {
    if let Ok(hook) = std::env::var("GOOSE_CLI_STATUS_HOOK") {
        let _ = std::process::Command::new(hook).arg(status).spawn();
    }
}

pub struct McpSpinners {
    bars: HashMap<String, ProgressBar>,
    log_spinner: Option<ProgressBar>,

    multi_bar: MultiProgress,
}

impl McpSpinners {
    pub fn new() -> Self {
        McpSpinners {
            bars: HashMap::new(),
            log_spinner: None,
            multi_bar: MultiProgress::new(),
        }
    }

    pub fn log(&mut self, message: &str) {
        let spinner = self.log_spinner.get_or_insert_with(|| {
            let bar = self.multi_bar.add(
                ProgressBar::new_spinner()
                    .with_style(
                        ProgressStyle::with_template("{spinner:.green} {msg}")
                            .unwrap()
                            .tick_chars("⠋⠙⠚⠛⠓⠒⠊⠉"),
                    )
                    .with_message(message.to_string()),
            );
            bar.enable_steady_tick(Duration::from_millis(100));
            bar
        });

        spinner.set_message(message.to_string());
    }

    pub fn update(&mut self, token: &str, value: f64, total: Option<f64>, message: Option<&str>) {
        let bar = self.bars.entry(token.to_string()).or_insert_with(|| {
            if let Some(total) = total {
                self.multi_bar.add(
                    ProgressBar::new((total * 100_f64) as u64).with_style(
                        ProgressStyle::with_template("[{elapsed}] {bar:40} {pos:>3}/{len:3} {msg}")
                            .unwrap(),
                    ),
                )
            } else {
                self.multi_bar.add(ProgressBar::new_spinner())
            }
        });
        bar.set_position((value * 100_f64) as u64);
        if let Some(msg) = message {
            bar.set_message(msg.to_string());
        }
    }

    pub fn hide(&mut self) -> Result<(), Error> {
        self.bars.iter_mut().for_each(|(_, bar)| {
            bar.disable_steady_tick();
        });
        if let Some(spinner) = self.log_spinner.as_mut() {
            spinner.disable_steady_tick();
        }
        self.multi_bar.clear()
    }
}

pub fn shorten_path(path: &str, debug: bool) -> String {
    if debug {
        return path.to_string();
    }

    let path_obj = Path::new(path);

    // Try to replace home directory with ~
    let display_path = if let Ok(home) = std::env::var("HOME") {
        if let Ok(stripped) = path_obj.strip_prefix(home) {
            format!("~/{}", stripped.display())
        } else {
            path.to_string()
        }
    } else {
        path.to_string()
    };

    // If path is still very long, shorten intermediate components
    if display_path.len() > 40 {
        let components: Vec<_> = Path::new(&display_path)
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect();

        if components.len() > 3 {
            let mut shortened = String::new();
            for (i, comp) in components.iter().enumerate() {
                if i == 0 && display_path.starts_with('/') {
                    // root
                } else if i == components.len() - 1 || i == components.len() - 2 {
                    // last two components
                    shortened.push_str(comp);
                } else {
                    // middle components - just first char
                    if let Some(c) = comp.chars().next() {
                        shortened.push(c);
                    }
                }
                if i < components.len() - 1 {
                    shortened.push('/');
                }
            }
            return shortened;
        }
    }

    display_path
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn test_short_paths_unchanged() {
        assert_eq!(shorten_path("/usr/bin", false), "/usr/bin");
        assert_eq!(shorten_path("/a/b/c", false), "/a/b/c");
        assert_eq!(shorten_path("file.txt", false), "file.txt");
    }

    #[test]
    fn test_debug_mode_returns_full_path() {
        assert_eq!(
            shorten_path("/very/long/path/that/would/normally/be/shortened", true),
            "/very/long/path/that/would/normally/be/shortened"
        );
    }

    #[test]
    fn test_home_directory_conversion() {
        // Save the current home dir
        let original_home = env::var("HOME").ok();

        // Set a test home directory
        env::set_var("HOME", "/Users/testuser");

        assert_eq!(
            shorten_path("/Users/testuser/documents/file.txt", false),
            "~/documents/file.txt"
        );

        // A path that starts similarly to home but isn't in home
        assert_eq!(
            shorten_path("/Users/testuser2/documents/file.txt", false),
            "/Users/testuser2/documents/file.txt"
        );

        // Restore the original home dir
        if let Some(home) = original_home {
            env::set_var("HOME", home);
        } else {
            env::remove_var("HOME");
        }
    }

    #[test]
    fn test_toggle_full_tool_output() {
        let mut output = SessionOutput::new();
        let initial = output.get_show_full_tool_output();

        let after_first_toggle = output.toggle_full_tool_output();
        assert_eq!(after_first_toggle, !initial);
        assert_eq!(output.get_show_full_tool_output(), after_first_toggle);

        let after_second_toggle = output.toggle_full_tool_output();
        assert_eq!(after_second_toggle, initial);
        assert_eq!(output.get_show_full_tool_output(), initial);
    }

    #[test]
    fn test_long_path_shortening() {
        assert_eq!(
            shorten_path(
                "/vvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvvv/long/path/with/many/components/file.txt",
                false
            ),
            "/v/l/p/w/m/components/file.txt"
        );
    }
}
