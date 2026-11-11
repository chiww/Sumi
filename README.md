<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/sumi-lockup-reverse.svg" />
    <img src="assets/brand/sumi-lockup-primary.svg" alt="Sumi" width="260" />
  </picture>
  <h1>Sumi「墨」</h1>
  <p><strong>落笔，即入心流。</strong></p>
  <p>基于 <a href="https://github.com/tw93/Kaku">Kaku</a> 打磨的个人 Mac 终端。<br/>给每天写命令、改代码的自己，留一个顺手的工作台。</p>
  <p><em>A terminal of your own. Customized from Kaku, built on WezTerm.</em></p>
  <p>
    <a href="https://github.com/chiww/Sumi/releases/latest">下载</a> ·
    <a href="docs/configuration.md">配置</a> ·
    <a href="docs/faq.md">FAQ</a> ·
    <a href="LICENSE.md">MIT License</a>
  </p>
</div>

## 和 Kaku 的不同

- **简体中文界面**：菜单、命令面板、确认对话框、`sumi config` 设置界面、AI 浮层都有中文。设置 `config.language = "zh-CN"`，或保持 `"auto"` 跟随系统。
- **行间距预设**：`sumi config` 的「行间距」（Line Height）提供 1.0–1.6 的常用档位，以及自定义输入。
- **独立身份**：独立的应用名、图标、Bundle ID（`com.github.chiww.sumi`）、命令（`sumi`）和配置目录（`~/.config/sumi`），可以和原版 Kaku 同时安装，互不干扰。
- **更新来源**：应用内检查更新指向本仓库的 Releases。

## 安装

从 [Releases](https://github.com/chiww/Sumi/releases/latest) 下载 `Sumi.dmg`，打开后把 Sumi 拖进「应用程序」。

当前构建未经 Apple 公证。第一次打开如果提示「无法验证开发者」，在 Finder 里右键 Sumi →「打开」，或执行：

```bash
xattr -cr /Applications/Sumi.app
```

首次启动会引导配置 shell 集成，之后在终端里可以直接使用 `sumi` 命令。

## 常用命令

| 命令 | 作用 |
| --- | --- |
| `sumi` | 打开主菜单 |
| `sumi config` | 设置界面：主题、字体、字号、行间距等 |
| `sumi ai` | 配置 AI 助手（使用你自己的 API Key 或 Codex 登录） |
| `sumi init` | 安装或更新 shell 集成 |
| `sumi doctor` | 诊断并生成诊断包 |
| `sumi reset` | 移除 Sumi 写入的 shell 配置 |
| `k` | 在任意终端里和 AI 对话 |

配置文件位于 `~/.config/sumi/sumi.lua`，兼容 WezTerm 风格的 Lua 配置。完整选项见 [docs/configuration.md](docs/configuration.md)。

## 从源码构建

```bash
make dev                 # 调试运行
./scripts/build.sh       # 生成 dist/Sumi.app 与 dist/Sumi.dmg
```

需要 Rust（版本见 `rust-toolchain.toml`）与 Xcode Command Line Tools。

## Credits

Sumi 从 [Kaku](https://github.com/tw93/Kaku)（Tw93）出发，Kaku 基于 [WezTerm](https://github.com/wezterm/wezterm)（Wez Furlong）。感谢两个项目的作者与贡献者。

本项目保留上游的 [LICENSE](LICENSE.md) 及相关第三方声明。Sumi 为个人二次开发版本，与 Kaku、WezTerm 官方无关。
