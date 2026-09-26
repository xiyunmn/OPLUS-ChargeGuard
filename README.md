# OPLUS 充电温控

面向 OPLUS 设备的充电温控模块，提供 Rust 控制核心与 WebUI。

**版本：v1.0.0 · 模块 ID：`charge_guard`**

## 功能

- 全局温控与充电独立预设，分别调整电池、CPU、GPU、DDR 温度目标。默认关闭全局温控、开启充电预设。
- 充电与相机状态事件监听，支持充电期间禁用 Horae 或根据相机状态智能调整。
- 支持事件驱动和循环维护两种写入模式，默认事件驱动。Cooling、Bouncing 等使用事件维护；shell-temp、emul_temp、game_opt 保留必要周期维护。
- WebUI 提供仪表盘、温度接口、后端详情和轮转日志，支持浅色／深色主题及 95%～115% 界面 DPI 调节。
- 支持手动停止、恢复与卸载；不携带 `system.prop`，不修改持久化温控属性。

## 使用

需要已取得 root 权限的 OPLUS ARM64 设备，Android 15 / API 35 及以上，以及支持模块安装的管理器；WebUI 需要相应的管理器入口或兼容宿主。

主要验证设备为 **PJZ110 / ColorOS 16**。其他 OPPO、OnePlus、realme 设备允许安装，后端按实际节点启用，兼容性与充电效果需自行验证。温度目标和被映射的接口读数不代表真实物理温度。

将 `*_magisk.zip` 交给模块管理器安装，再从 WebUI 配置。详细日志默认关闭，可在排查问题时开启并导出。

## 本地构建

支持 Windows 与 Linux。准备 Python 3.11+、Rust 1.85.1（含 `aarch64-linux-android` 目标）、JDK 17，以及 Android SDK：平台 35、Build Tools 36.0.0、NDK 27.3.13750724。

设置 `ANDROID_HOME` 和 `JAVA_HOME` 后运行：

```sh
python tools/build.py build --channel beta
python tools/build.py build --channel release
```

也可使用 `--sdk`、`--ndk`、`--jdk` 指定路径。产物位于 `target/dist/`，构建不会改写工作区版本信息。依赖按锁文件获取，需要离线构建时显式添加 `--offline`。

## 协议

本项目采用 [MIT License](LICENSE)。
