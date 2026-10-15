//! OTools 宿主桥：把「文件从哪来、存到哪去」换成 OTools 宿主的对话框与磁盘读写。
//!
//! 上游 `apps/designcraft-web` 在浏览器里没有文件系统：打开靠 `<input type=file>`，
//! 保存靠浏览器下载。OTools 宿主注入的 `window.otools` 提供了真正的系统文件对话框与
//! `readHostFile` / `writeHostFile`（底层是宿主的 `tools_webview_read_file` /
//! `tools_webview_write_file`，无路径白名单限制），所以本模块是插件**唯一**需要新写的
//! Rust 代码：其余全部复用上游 crate。
//!
//! 与上游的对接点都在上游设计好的 `Services` 钩子上，上游源码零改动：
//! - `Services::open_async` —— 弹宿主的打开对话框，读到字节推进 inbox
//! - `Services::pick_save`  —— 记住「这次要弹另存对话框」，返回建议文件名
//! - `Services::write` / `Services::download` —— 保存时把字节交给宿主写盘
//!
//! JS 侧的实现见 `shell/index.html` 里的 `window.designcraftOtools`。

use std::cell::{Cell, RefCell};

use base64::Engine as _;
use designcraft_ui_egui::{ImportRequest, ImportedFile, Inbox};
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::js_sys;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = designcraftOtools, js_name = "available")]
    fn host_available() -> bool;

    #[wasm_bindgen(js_namespace = designcraftOtools, js_name = "theme")]
    fn host_theme() -> String;

    /// 宿主界面语言（BCP 47，如 `zh-CN`）：OTools 注入到插件 webview 的
    /// `window.__OTOOLS_LOCALE__.locale` / `document.documentElement.lang`。
    #[wasm_bindgen(js_namespace = designcraftOtools, js_name = "language")]
    fn host_language_raw() -> String;

    /// `{title, extensions}` → `[{name, path, dataBase64}]`。
    #[wasm_bindgen(js_namespace = designcraftOtools, js_name = "openFiles")]
    fn host_open_files(request: JsValue) -> js_sys::Promise;

    /// `{name, dataBase64, forcePick}` → `{path}` 或 `null`（用户取消）。
    #[wasm_bindgen(js_namespace = designcraftOtools, js_name = "saveFile")]
    fn host_save_file(request: JsValue) -> js_sys::Promise;
}

thread_local! {
    /// 打开的文件交给它：下一帧由 `WebShell::logic` 交给应用（上游的 Inbox 机制）。
    static OUTBOX: RefCell<Option<(Inbox, egui::Context)>> = const { RefCell::new(None) };
    /// 下一次保存是否**必须**弹另存对话框（「导出/另存为」置真，「保存」用上次路径）。
    static FORCE_PICK: Cell<bool> = const { Cell::new(false) };
}

/// 宿主是否可用（不在 OTools 里时退回上游的浏览器行为：下载文件）。
pub fn available() -> bool {
    host_available()
}

/// OTools 的界面主题，用于给 egui 一个初始主题（`""` = 跟随系统）。
pub fn theme() -> String {
    host_theme()
}

/// 宿主界面语言（BCP 47，如 `zh-CN`）：跟随 OTools 宿主语言切换。
pub fn host_language() -> String {
    host_language_raw()
}

/// 社区/外部链接（Discord、ArtCraft 官网、上游 GitHub）：在 OTools 插件里点这些
/// 会把用户拽到外部，让插件不纯粹。语言切换监听与链接屏蔽用到它。
pub fn is_community_url(url: &str) -> bool {
    url.contains("discord.gg") || url.contains("getartcraft.com") || url.contains("github.com/storytold")
}

pub fn install(inbox: Inbox, ctx: egui::Context) {
    OUTBOX.with(|slot| *slot.borrow_mut() = Some((inbox, ctx)));
}

fn push(request: ImportRequest, name: String, bytes: Vec<u8>) {
    OUTBOX.with(|slot| {
        if let Some((inbox, ctx)) = slot.borrow().as_ref() {
            inbox.lock().unwrap_or_else(|e| e.into_inner()).push(ImportedFile { request, name, bytes });
            ctx.request_repaint();
        }
    });
}

/// 每种导入用途对应的扩展名过滤，与上游 `apps/designcraft-web` 的 `open_async` 一致。
fn extensions_for(purpose: &str) -> Vec<String> {
    match purpose {
        "swatches" => vec!["ase".into()],
        "place" => vec![
            "png".into(), "jpg".into(), "jpeg".into(), "gif".into(), "webp".into(), "tif".into(), "tiff".into(), "bmp".into(),
            "psd".into(), "svg".into(), "pdf".into(), "ai".into(), "txt".into(), "docx".into(), "rtf".into(), "md".into(),
            "xlsx".into(), "idml".into(), "designcraft".into(),
        ],
        _ => vec!["designcraft".into(), "idml".into()],
    }
}

fn title_for(purpose: &str) -> String {
    match purpose {
        "swatches" => "载入色板".into(),
        "place" => "置入图片".into(),
        _ => "打开文档".into(),
    }
}

/// `Services::open_async`：弹宿主的打开对话框，读到的字节在下一帧进 inbox。
pub fn pick_open(request: ImportRequest) {
    let payload = open_request(&title_for(&request.purpose), extensions_for(&request.purpose));
    wasm_bindgen_futures::spawn_local(async move {
        match open_files(&payload).await {
            Ok(files) => {
                for (name, bytes) in files {
                    push(request.clone(), name, bytes);
                }
            }
            Err(e) => log::error!("打开失败：{e}"),
        }
    });
}

/// `ImportRequest` 是不是 `Clone`？不是（上游只按需移动）。这里手动重建一份。
fn request_clone(request: &ImportRequest) -> ImportRequest {
    ImportRequest { purpose: request.purpose.clone(), target: request.target }
}

pub fn pick_open_request(request: &ImportRequest) {
    // ImportRequest 不可 Clone：先拷出需要的字段再交出去。
    pick_open(request_clone(request));
}

/// `Services::pick_save`：置「下次弹另存对话框」并返回建议的文件名。
pub fn pick_save(name: &str) -> Option<String> {
    FORCE_PICK.with(|flag| flag.set(true));
    Some(name.to_string())
}

/// `Services::write` / `Services::download`：把字节交给宿主写盘。
///
/// 上游的钩子是同步的 `fn`，而宿主的对话框是 Promise，所以这里发起一次后台任务并
/// 立刻返回 `Ok`（与上游网页版「下载已开始即算已保存」的语义一致）。
pub fn deliver(name: &str, bytes: &[u8]) -> Result<(), String> {
    if name.trim().is_empty() {
        return Err("需要一个文件名".into());
    }
    let force = FORCE_PICK.with(|flag| flag.replace(false));
    let request = save_request(&file_name(name), &base64::engine::general_purpose::STANDARD.encode(bytes), force);
    // spawn_local 要求 'static：`name` 的所有权必须带进异步块，不能借用函数参数。
    let name = name.to_string();
    wasm_bindgen_futures::spawn_local(async move {
        match save_file(&request).await {
            Ok(Some(path)) => log::info!("saved {name} -> {path}"),
            Ok(None) => log::error!("保存已取消：{name}"),
            Err(e) => log::error!("保存失败：{e}"),
        }
    });
    Ok(())
}

fn file_name(path: &str) -> String {
    std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "Document.designcraft".into())
}

fn open_request(title: &str, extensions: Vec<String>) -> JsValue {
    let request = js_sys::Object::new();
    let exts = js_sys::Array::new();
    for e in extensions {
        exts.push(&JsValue::from_str(&e));
    }
    let _ = js_sys::Reflect::set(&request, &JsValue::from_str("title"), &JsValue::from_str(title));
    let _ = js_sys::Reflect::set(&request, &JsValue::from_str("extensions"), &exts);
    request.into()
}

async fn open_files(request: &JsValue) -> Result<Vec<(String, Vec<u8>)>, String> {
    let value = JsFuture::from(host_open_files(request.clone())).await.map_err(js_error)?;
    let mut out = Vec::new();
    for entry in js_sys::Array::from(&value).iter() {
        let name = field(&entry, "name").unwrap_or_else(|| "Document.designcraft".to_string());
        let data = field(&entry, "dataBase64").unwrap_or_default();
        let bytes = base64_decode(&data)?;
        out.push((name, bytes));
    }
    Ok(out)
}

fn save_request(name: &str, data: &str, force_pick: bool) -> JsValue {
    let request = js_sys::Object::new();
    let _ = js_sys::Reflect::set(&request, &JsValue::from_str("name"), &JsValue::from_str(name));
    let _ = js_sys::Reflect::set(&request, &JsValue::from_str("dataBase64"), &JsValue::from_str(data));
    let _ = js_sys::Reflect::set(&request, &JsValue::from_str("forcePick"), &JsValue::from_bool(force_pick));
    request.into()
}

async fn save_file(request: &JsValue) -> Result<Option<String>, String> {
    let value = JsFuture::from(host_save_file(request.clone())).await.map_err(js_error)?;
    if value.is_null() || value.is_undefined() {
        return Ok(None);
    }
    Ok(field(&value, "path"))
}

fn base64_decode(data: &str) -> Result<Vec<u8>, String> {
    if data.is_empty() {
        return Err("宿主返回的文件内容为空".into());
    }
    base64::engine::general_purpose::STANDARD.decode(data).map_err(|e| format!("宿主返回的内容不是合法的 base64：{e}"))
}

fn field(value: &JsValue, key: &str) -> Option<String> {
    js_sys::Reflect::get(value, &JsValue::from_str(key)).ok()?.as_string()
}

fn js_error(e: JsValue) -> String {
    e.as_string().unwrap_or_else(|| format!("{e:?}"))
}
