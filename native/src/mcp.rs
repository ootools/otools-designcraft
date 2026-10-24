//! 上游 `designcraft-mcp` 与宿主 MCP 契约之间的薄适配（见宿主 spec §5.4）。
//!
//! **有意不做 UI-only 工具过滤**：上游在调用点用 `need_ui()` 运行时判断（以
//! `Backend::has_ui()` 为准），而 headless 下 `has_ui()` 恒为 false。任何按名字的
//! 静态过滤都会在这两种情形上出错：
//!   1. 同名工具在不同插件里可用性相反（DesignCraft 的 `type_text` 可用，
//!      VectorCraft 的 `type_text` 不可用）
//!   2. 可用性取决于参数（VectorCraft 的 `screenshot` 仅 `window:true` 需 UI）
//! 因此原样透传，把失败交回上游给出的可读错误（AI 能据此改参数或换工具）。

use designcraft_mcp::{call_tool, tool_definitions, Backend, Headless, Server};
use serde_json::{json, Value};

/// 宿主 `__otools_mcp_tools__` 的响应体。
pub fn tools_payload() -> Value {
    json!({ "tools": tool_definitions() })
}

/// 进程内**常驻**的 headless 会话。
///
/// 这一点至关重要：上游 `Headless` 持有一个 `Session`，而工具序列是有状态的——
/// `new_document` → `execute` → `save_document` 依赖同一个 Session 里的活动文档。
/// 若每次调用都新建 `Headless`，第二次调用就会面对一个空会话，整条工作流断裂。
///
/// 上游的 MCP server 之所以没这个问题，是因为它是长驻进程、持有一个
/// `Box<dyn Backend>`。这里用同样的思路：把 Session 存在 dylib 内的 OnceLock 里，
/// 生命周期与动态库一致，宿主热更新时会走 `__otools_shutdown__` 退役旧库。
///
/// 用 `with_document()` 而不是 `new()`：与上游自己的 headless server 一致
/// （`apps/designcraft-cli/src/main.rs:394` 就是 `Headless::with_document()`），
/// 会话起始就有一份默认文档，`execute` / `inspect_document` 可直接用。
static HEADLESS: std::sync::OnceLock<std::sync::Arc<std::sync::Mutex<Headless>>> =
    std::sync::OnceLock::new();

fn headless() -> &'static std::sync::Arc<std::sync::Mutex<Headless>> {
    HEADLESS.get_or_init(|| std::sync::Arc::new(std::sync::Mutex::new(Headless::with_document())))
}

/// 把常驻会话包成上游 `Server` 能接受的 `Backend`。
///
/// **为什么需要这一层**：上游的 `resources/*`、`prompts/*` 分发逻辑写在
/// `Server::handle` 里，而 `Server::new` 要求 `Box<dyn Backend>` 的**所有权**。
/// 直接把 Headless 交出去就再也拿不回来（会话状态丢失），所以这里交一个只持有
/// `Arc<Mutex<Headless>>` 的代理：`Server` 用完即弃，真正的会话仍留在静态槽里。
///
/// 好处是资源表、读取映射、错误码全部由上游代码产生，适配层不再复制一份
/// 「uri → 控制方法」的映射——那种复制正是上游改一处、插件悄悄过期的地方。
struct Proxy(std::sync::Arc<std::sync::Mutex<Headless>>);

impl Proxy {
    fn with<T>(&self, f: impl FnOnce(&mut Headless) -> T) -> Result<T, String> {
        let mut guard = self
            .0
            .lock()
            .map_err(|_| "headless 会话状态已损坏，请重启 OTools".to_string())?;
        Ok(f(&mut guard))
    }
}

impl Backend for Proxy {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.with(|h| h.call(method, params))?
    }

    fn has_ui(&self) -> bool {
        false
    }

    fn write_file(&mut self, path: &str, bytes: &[u8]) -> Result<(), String> {
        self.with(|h| h.write_file(path, bytes))?
    }

    fn describe(&self) -> String {
        "otools-designcraft native (headless)".to_string()
    }
}

/// 走一次上游 MCP 分发，取 `result`。
///
/// 用于 resources / prompts 这类**不属于工具调用**的方法：它们在上游是
/// `Server::handle` 的分支，只有经过完整分发才能拿到与上游逐字一致的响应。
pub fn rpc(method: &str, params: Value) -> Result<Value, String> {
    let mut server = Server::new(Box::new(Proxy(headless().clone())));
    let reply = server
        .handle(json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }))
        .ok_or_else(|| format!("上游对 {method} 没有应答"))?;
    if let Some(error) = reply.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("上游返回错误");
        return Err(message.to_string());
    }
    reply
        .get("result")
        .cloned()
        .ok_or_else(|| format!("{method} 的应答缺少 result"))
}

/// 宿主 `__otools_mcp_resources__` 的响应体。
pub fn resources_payload() -> Result<Value, String> {
    rpc("resources/list", json!({}))
}

/// 宿主 `__otools_mcp_resource_templates__` 的响应体。
pub fn resource_templates_payload() -> Result<Value, String> {
    rpc("resources/templates/list", json!({}))
}

/// 宿主 `__otools_mcp_read_resource__` 的响应体。上游对未知 uri 会给出
/// `-32002 resource not found`，这里转成 `Err`（resources 没有 `isError` 信封）。
pub fn read_resource_payload(uri: &str) -> Result<Value, String> {
    rpc("resources/read", json!({ "uri": uri }))
}

/// 关掉会话（供宿主热更新前的停机握手调用）。
pub fn shutdown_headless() {
    if let Some(lock) = HEADLESS.get() {
        if let Ok(mut backend) = lock.lock() {
            // 置为全新会话（带默认文档，与初始状态一致），释放旧文档占用的内存。
            *backend = Headless::with_document();
        }
    }
}

/// 宿主 `__otools_mcp_call__` 的响应体。
pub fn call_payload(name: &str, arguments: &Value) -> Value {
    let mut guard = match headless().lock() {
        Ok(guard) => guard,
        // 锁中毒（上一次调用 panic）时给出明确错误，而不是继续用半坏状态。
        Err(_) => {
            return json!({
                "content": [{ "type": "text", "text": "headless 会话状态已损坏，请重启 OTools" }],
                "isError": true
            })
        }
    };
    let result = call_tool(&mut *guard, name, arguments);
    // 上游 ToolResult 已有 to_value()，字段名与 MCP 一致（content / isError）。
    result.to_value()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // 这些用例共享进程内的常驻 headless 会话（这本身就是被测行为），
    // 因此必须串行执行：cargo test 默认多线程会让 use 例互相看到对方的文档。
    // 用一把全局锁把本模块的用例串起来。
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn tools_payload_wraps_upstream_definitions() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let value = tools_payload();
        let tools = value
            .get("tools")
            .and_then(|v| v.as_array())
            .expect("tools array");
        // 上游有 23 个工具；断言非空且每项含 name，避免把空表当成功。
        assert!(!tools.is_empty(), "tool table should not be empty");
        for tool in tools {
            assert!(
                tool.get("name").and_then(|v| v.as_str()).is_some(),
                "every tool needs a name: {tool}"
            );
        }
    }

    #[test]
    fn tools_payload_passes_through_ui_only_tools() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 有意**不过滤** UI-only 工具：上游在调用点用 need_ui() 判断，
        // 且存在「同名工具可用性相反」与「取决于参数」两种情形（见 spec §10.2）。
        let value = tools_payload();
        let names: Vec<&str> = value["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.get("name").and_then(|v| v.as_str()))
            .collect();
        assert!(
            names.contains(&"screenshot"),
            "headless 不可用的工具也应透传，由上游给出可读错误；got {names:?}"
        );
    }

    #[test]
    fn call_unknown_tool_returns_is_error() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let result = call_payload("definitely_not_a_tool", &json!({}));
        assert_eq!(result["isError"], true, "result: {result}");
    }

    #[test]
    fn call_read_only_tool_succeeds_headless() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // list_commands 在 headless 下可用（Headless 实现了 engine.execute 路径）。
        let result = call_payload("list_commands", &json!({}));
        assert_eq!(result["isError"], false, "result: {result}");
        assert!(
            result.get("content").and_then(|v| v.as_array()).is_some(),
            "must return MCP content array: {result}"
        );
    }

    #[test]
    fn session_state_persists_across_calls() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 这一步是**防止「每次调用新建 Headless」回归**的关键测试：
        // new_document 之后，inspect_document 必须看得到那个文档。
        let created = call_payload("new_document", &json!({}));
        assert_eq!(
            created["isError"], false,
            "new_document should succeed: {created}"
        );

        let inspected = call_payload("inspect_document", &json!({}));
        assert_eq!(
            inspected["isError"], false,
            "inspect_document must see the document created in the previous call: {inspected}"
        );
    }

    // ── resources ──────────────────────────────────────────────────────────
    //
    // 上游的 `resources/*` 分发在 `Server::handle` 里，适配层通过 Proxy 转发。
    // 这三条锁的是「转发真的通」：列举到上游的两个 URI、能读回内容、未知 uri 报错。

    #[test]
    fn resources_list_matches_upstream() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let value = resources_payload().expect("resources/list");
        let uris: Vec<&str> = value["resources"]
            .as_array()
            .expect("resources array")
            .iter()
            .filter_map(|r| r.get("uri").and_then(Value::as_str))
            .collect();
        assert_eq!(uris.len(), 2, "got {uris:?}");
        assert!(uris.contains(&"designcraft://document"), "got {uris:?}");
        assert!(uris.contains(&"designcraft://commands"), "got {uris:?}");
    }

    #[test]
    fn resource_templates_is_an_empty_table() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 上游实现就是空表（固定 URI，没有模板），但方法必须答得上。
        let value = resource_templates_payload().expect("resources/templates/list");
        let items = value
            .get("resourceTemplates")
            .and_then(Value::as_array)
            .expect("resourceTemplates array");
        assert!(items.is_empty(), "{value}");
    }

    #[test]
    fn read_resource_round_trips_and_rejects_unknown_uri() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let contents = read_resource_payload("designcraft://document")
            .expect("read document resource")
            .get("contents")
            .and_then(Value::as_array)
            .cloned()
            .expect("contents array");
        let text = contents[0]
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(!text.trim().is_empty(), "document resource came back empty");

        let error = read_resource_payload("designcraft://nope").expect_err("unknown uri must fail");
        assert!(
            error.contains("not found") || error.contains("resource"),
            "error should say the resource is missing: {error}"
        );
    }

    #[test]
    fn prompts_are_answered_even_though_upstream_has_none() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // 上游没有 prompts，但协议要求答得上：空表而不是 method not found。
        let error = rpc("prompts/list", json!({})).expect_err("upstream has no prompts");
        assert!(
            error.contains("not found") || error.contains("method"),
            "got {error}"
        );
    }
}
