# DesignCraft · OTools 插件

把 [storytold/designcraft](https://github.com/storytold/designcraft)（开源、纯 Rust 重写的 InDesign 替代品）
移植为 OTools 插件：上游的 UI 原封不动编译到 WebAssembly，外壳把「文件从哪来、存到哪去」
换成 OTools 宿主的对话框与磁盘读写。

- 插件目录：`plugins/otools-designcraft`（在 [ootools/OTools](https://github.com/ootools/OTools) 主仓里）
- 上游源码：**不入库**，见 `upstream.pin` + `scripts/fetch-upstream.mjs`

## MCP

插件通过宿主 MCP 网关暴露上游 `designcraft-mcp` 的 26 个工具，以及上游的 2 个资源：

| 能力 | 内容 |
| --- | --- |
| tools | 26 个（`mcp.expose` 是设置页的默认勾选种子，其余可显式授权） |
| resources | `designcraft://document`、`designcraft://commands` |
| resourceTemplates | 无（上游返回空表，但方法答得上） |
| prompts | 无（上游没有这个能力；适配层答空表） |

`native/` 用 `Proxy` 把常驻的 headless 会话包成上游 `Server` 能接受的 `Backend`：
`Server::new` 要求 `Box<dyn Backend>` 的**所有权**，直接把会话交出去就再也拿不回来，
所以每次请求新建一个只持有 `Arc<Mutex<Headless>>` 的代理 Server。这样资源表、读取映射、
错误码全部由上游 `Server::handle` 产生，适配层没有会过期的副本。

```bash
cargo test --release --manifest-path native/Cargo.toml
cargo build --release --manifest-path native/Cargo.toml
node scripts/smoke-mcp.mjs ..          # 参数是各 craft 仓库的共同父目录
```

## 上游版本

打包所用的上游提交。`upstream.pin` 是唯一真源，这张表用来回溯「哪个插件版本打的是哪份上游代码」，
每次发布都要在这里加一行。

| 插件版本 | 上游标签 | 上游提交 | 打包日期 |
| --- | --- | --- | --- |
| 0.1.5 | v0.10.0 | `15a2f3c8c88ae2e011e5912149ac67460ea6d949` | 2026-10-24 |
| 0.1.4 | v0.10.0 | `15a2f3c8c88ae2e011e5912149ac67460ea6d949` | 2026-10-21 |
| 0.1.3 | — | `d07cbbe94c2762865eaefeff4ebfd1dbada180a0` | — |

## 构建

```bash
unset RUSTC_WRAPPER                 # 它会把 cargo 指向旧 rustc
node scripts/fetch-upstream.mjs     # 拉取 pinned 的上游到 upstream/
node scripts/build-wasm.mjs         # trunk build --release → dist/
```

需要 rustc >= 1.95 与 wasm32-unknown-unknown 目标（egui 0.36 的要求）。

详细约定见 [AGENTS.md](./AGENTS.md)。
