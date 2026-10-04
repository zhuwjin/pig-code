# 供应商图标

预设供应商选择弹窗使用的品牌图标。来源：ZCode 仓库
（`packages/ui/src/assets/provider-icons/`，Apache-2.0） curated 的
官方品牌标识，出处记录见该目录的 `model-provider-logo-sources.json`：

| 文件 | 品牌 | 原始来源 |
|---|---|---|
| bigmodel.svg | 智谱 BigModel | 官方蓝色渐变矢量（透明底） |
| zai.png | Z.ai | 官方标识 |
| kimi.png | Kimi (Moonshot) | 官方直角图标 80×80 |
| minimax.png | MiniMax | 官方品牌包 200×200 SVG 渲染 80×80 |
| deepseek.png | DeepSeek | 官方 GitHub 组织头像 |
| alibaba.png | 阿里云百炼 | 阿里云 GitHub 组织方形符号 |
| mimo.png | Xiaomi MiMo | 官方 256×256 内嵌源图缩至 80×80 |
| anthropic.png | Anthropic | 官网 webclip 256×256 |
| openai.png | OpenAI | 官方帮助中心图标 1024×1024 缩至 80×80 |
| xai.png | xAI | 官方 GitHub 组织头像 |
| openrouter-light.svg / openrouter-dark.svg | OpenRouter | 官方品牌页 grape（亮底）/ volt（暗底）方形字形，按主题选用 |

经 rust-embed 嵌入二进制（`src/assets.rs`），资产路径前缀
`provider-icons/`。品牌商标版权归各供应商所有。
