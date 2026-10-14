//! DesignCraft 的 OTools 插件外壳。
//!
//! 本文件改编自上游 `apps/designcraft-web/src/web.rs`（上游 eframe web runner 的外壳）。
//! 改动都在「文件从哪来、存到哪去」上：
//!
//! 1. `services()` 的 `open_async` / `pick_save` / `write` / `download` 走 [`otools`]
//! 2. 首次启动时用 OTools 的界面主题给 egui 一个初始亮度
//! 3. 不在 OTools 里时（把 dist 拖进普通浏览器）退回浏览器下载
//!
//! 上游 `crates/*` 一律不改：它们按 upstream.pin 的 commit 拉取，保持原样才能干净地跟上上游。

#[cfg(target_arch = "wasm32")]
mod otools;

#[cfg(target_arch = "wasm32")]
mod web {
    use designcraft_engine::Session;
    use designcraft_ui_egui::{DesignApp, ImportRequest, ImportedFile, Inbox, Services, theme::Brightness};
    use wasm_bindgen::JsCast as _;

    const CANVAS_ID: &str = "designcraft_canvas";
    const LOADING_ID: &str = "designcraft_loading";

    /// 包一层应用：读拖放进来的文件（浏览器讲异步），通过 inbox 交给应用。
    struct WebShell {
        app: DesignApp,
        inbox: Inbox,
    }

    impl eframe::App for WebShell {
        fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            let dropped = ctx.input_mut(|i| std::mem::take(&mut i.raw.dropped_files));
            for f in dropped {
                let request = self.app.import_request("drop");
                let inbox = self.inbox.clone();
                let ctx = ctx.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let name = f.path().file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "dropped".into());
                    match f.bytes_async().await {
                        Ok(bytes) => {
                            inbox.lock().unwrap_or_else(|e| e.into_inner()).push(ImportedFile { request, name, bytes });
                            ctx.request_repaint();
                        }
                        Err(e) => log::error!("读拖放文件 {name} 失败：{e}"),
                    }
                });
            }
            self.app.logic(ctx);
        }

        fn raw_input_hook(&mut self, ctx: &egui::Context, raw: &mut egui::RawInput) {
            // 浏览器把未知键名当文本事件；插件里 eframe 直接给键名，不需要这层修正。
            designcraft_ui_egui::drop_key_name_text(raw, ctx.text_edit_focused());
            self.app.raw_input_hook(raw);
        }

        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            self.app.ui(ui);
        }
    }

    fn query() -> String {
        web_sys::window().and_then(|w| w.location().search().ok()).unwrap_or_default()
    }

    fn has_flag(query: &str, key: &str) -> bool {
        // 与上游 browser_policy.rs 一致：键精确匹配，值忽略（?sample=false 也算开）。
        query.trim_start_matches('?').split('&').any(|p| p == key || p.starts_with(&format!("{key}=")))
    }

    /// 宿主服务：在 OTools 里用宿主的对话框与磁盘；否则退回上游网页版的行为。
    fn services(inbox: Inbox) -> Services {
        let shared = inbox.clone();
        if crate::otools::available() {
            let open_inbox = shared.clone();
            return Services {
                open_async: Some(Box::new(move |request: ImportRequest| {
                    let _ = &open_inbox;
                    crate::otools::pick_open_request(&request);
                })),
                pick_save: Some(Box::new(crate::otools::pick_save)),
                write: Some(Box::new(|name: &str, bytes: &[u8]| {
                    if let Err(e) = crate::otools::deliver(name, bytes) {
                        log::error!("写 {name} 失败：{e}");
                    }
                    Ok(())
                })),
                download: Some(Box::new(|name: &str, bytes: &[u8]| {
                    if let Err(e) = crate::otools::deliver(name, bytes) {
                        log::error!("保存 {name} 失败：{e}");
                    }
                })),
                inbox: Some(shared),
                ..Services::default()
            };
        }
        Services {
            pick_save: Some(Box::new(|name: &str| Some(name.to_string()))),
            write: Some(Box::new(|name: &str, bytes: &[u8]| download(name, bytes))),
            download: Some(Box::new(|name: &str, bytes: &[u8]| {
                if let Err(e) = download(name, bytes) {
                    log::error!("download of {name} failed: {e}");
                }
            })),
            inbox: Some(inbox),
            ..Services::default()
        }
    }

    /// 不在 OTools 里时的兜底：浏览器下载（这段改自上游 `apps/designcraft-web` 的同名函数）。
    fn download(path: &str, bytes: &[u8]) -> Result<(), String> {
        let js = |e: wasm_bindgen::JsValue| format!("{e:?}");
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "designcraft".into());
        let window = web_sys::window().ok_or("no window")?;
        let document = window.document().ok_or("no document")?;
        let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
        let blob = web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &web_sys::BlobPropertyBag::new()).map_err(js)?;
        let url = web_sys::Url::create_object_url_with_blob(&blob).map_err(js)?;
        let a: web_sys::HtmlAnchorElement = document.create_element("a").map_err(js)?.dyn_into().map_err(|_| "not an anchor")?;
        a.set_href(&url);
        a.set_download(&name);
        let body = document.body().ok_or("no body")?;
        body.append_child(&a).map_err(js)?;
        a.click();
        a.remove();
        Ok(())
    }

    pub fn start() {
        eframe::WebLogger::init(log::LevelFilter::Info).ok();
        let query = query();
        wasm_bindgen_futures::spawn_local(async move {
            let Some(document) = web_sys::window().and_then(|w| w.document()) else { return };
            let Some(canvas) = document.get_element_by_id(CANVAS_ID).and_then(|e| e.dyn_into::<web_sys::HtmlCanvasElement>().ok()) else {
                log::error!("missing <canvas id=\"{CANVAS_ID}\">");
                return;
            };
            let mut options = eframe::WebOptions::default();
            if has_flag(&query, "webgl")
                && let eframe::egui_wgpu::WgpuSetup::CreateNew(create) = &mut options.wgpu_options.wgpu_setup
            {
                create.instance_descriptor.backends = eframe::wgpu::Backends::GL;
            }
            let result = eframe::WebRunner::new()
                .start(
                    canvas,
                    options,
                    Box::new(move |cc| {
                        let inbox: Inbox = Inbox::default();
                        let mut app = DesignApp::new(Session::new(), services(inbox.clone()));
                        // 第一次打开跟随 OTools 的界面主题（插件 webview 的
                        // prefers-color-scheme 不一定等于宿主主题）。
                        if let Some(brightness) = Brightness::parse(&crate::otools::theme()) {
                            app.ui.brightness = brightness;
                            app.restyle = true;
                        }
                        if has_flag(&query, "sample") {
                            let _ = app.run("file.newSample", serde_json::json!({}));
                        }
                        crate::otools::install(inbox.clone(), cc.egui_ctx.clone());
                        Ok(Box::new(WebShell { app, inbox }))
                    }),
                )
                .await;
            if let Some(el) = document.get_element_by_id(LOADING_ID) {
                match result {
                    Ok(()) => el.remove(),
                    Err(e) => el.set_inner_html(&format!("<p>DesignCraft 启动失败：{e:?}</p><p>需要支持 WebGPU 或 WebGL2 的浏览器。</p>")),
                }
            }
        });
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {
    web::start();
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("otools-designcraft 只在浏览器里运行：在 shell/ 目录用 `trunk build --release` 构建");
}
