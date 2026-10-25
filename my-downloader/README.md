# my-downloader

Aria2-Harness 的前端与宿主源码：

- `src/` —— Vue 3 + Pinia + naive-ui 界面，经命名管道 IPC 调用宿主
- `src-host/` —— Rust 宿主（`aria2-harness` UI 进程 + `aria2-service` 提权服务）

构建与目录说明见仓库根目录的 [README](../README.md)。
