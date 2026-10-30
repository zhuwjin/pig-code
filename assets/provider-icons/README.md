# Provider icons

Preset and selectable provider brand icons come from [LobeHub Icons](https://lobehub.com/icons)
(MIT, `@lobehub/icons` / `lobehub/lobe-icons`). The files are the stock SVGs
from LobeHub 6.7.0 `packages/static-svg/icons/`, one file per catalog id:
`lobe/{id}.svg` (the `{id}-color.svg` artwork when LobeHub ships one, otherwise
the mono `{id}.svg`). No artwork or attribute changes.

## Catalogs

Two tables are generated from `src/toc.json` into
`crates/pig-app/src/settings/provider_icons_data.rs`; `provider_icons.rs` adds
the display aliases, search, and the merged picker catalog. They share one SVG
per id (220 distinct files):

- **Provider table (`PROVIDER_ICONS`, 152)** — all 145 LobeHub `provider`
  icons (OpenRouter, SiliconCloud, Azure, AWS, …) plus the shared vendor brands
  Kimi, Z.ai, DeepSeek, MiniMax, Xiaomi MiMo, Qwen, and StepFun.
- **Model table (`MODEL_ICONS`, 78)** — all 73 LobeHub `model` icons plus Kimi
  and the provider-owned model brands OpenAI, Meta, Microsoft, and Nvidia.

The picker shows the **union** of both tables (deduplicated by id), so a
provider can use any model mark and vice versa. The split only records
provenance and initial ordering; `all_icons()` is what the picker searches.
Shared brands use the same artwork on both sides, e.g. DeepSeek/Kimi/MiniMax/
Xiaomi MiMo/Qwen/StepFun appear in both tables.

| Preset | Catalog id |
|---|---|
| BigModel Coding Plan / BigModel API | `zhipu` |
| Z.ai Coding Plan / Z.ai API | `zai` |
| Kimi | `kimi` |
| MiniMax | `minimax` |
| DeepSeek | `deepseek` |
| Alibaba Cloud Bailian (China / International) | `bailian` |
| Xiaomi MiMo | `xiaomimimo` |
| Anthropic | `anthropic` |
| OpenAI | `openai` |
| xAI | `xai` |
| OpenRouter | `openrouter` |
| Ollama Cloud | `ollama` |
| OpenCode Go | `opencode` |
| CommandCode | `commandcode` |

## Selection and fallback

Icons are **explicit only** — there is no heuristic or preset-name matching.
`ProviderConfig.icon` and `ModelConfig.icon` store a chosen catalog id;
preset-created providers and preset models write their brand ids into the config.
Resolution is: valid explicit id → **server glyph** (providers, Lucide `Server`
on a muted tile) or **Lucide `Brain`** (models). Unknown or removed ids render
the default glyph. There is no legacy remapping: configs that still store an old
icon id keep rendering that artwork until the user picks a new one.

The picker opens by hovering a provider/model icon (scrim + pencil) and clicking
it. It shows the **merged provider + model catalog** with one **Default** card
first: selecting it clears the stored id, so the server/brain glyph is shown.
Search covers ids, titles, and aliases from both tables.

The composer's model selector uses the same explicit ids. Each provider group
starts with a disabled header row carrying the provider badge (Server fallback)
plus the provider name — upstream `CommandGroup` headings are text-only, so this
custom row replaces the text heading. Model rows lead with the model badge
(Brain fallback). Header keywords include every model in the group so the
provider header stays visible while searching.

## Rendering

`img()` cannot render these sharply on its own: its SVG loader rasterizes ONE
texture at the SVG's intrinsic size ×2 (LobeHub ships `1em` ≈ 16px, so a 32px
texture) and then scales that texture to the badge. `svg()`/`Icon` would
rasterize at the paint bounds, but it is monochrome by design and would flatten
the brand colors.

The settings page therefore draws icons with `ProviderIcon`
(`src/settings/providers.rs`), a `RenderOnce` element that sees the window at
render time and asks `assets::provider_icon_texture` for a texture at
`logical size × display scale` (e.g. 20px × 1.5 = 30px), cached per
(icon id, px, dark) in `src/provider_icon.rs`'s thread-local texture cache. That
gives a 1:1 texture with the painted badge and keeps full color. Dark themes replace the
SVG's `currentColor` tokens with `#FFFFFF` before parsing (Kimi's blue dot and
other explicit colors stay); parse failures fall back to the plain `img()`
path (soft, but visible).

Assets are embedded into the binary by rust-embed (`src/assets.rs`), with the
`provider-icons/` prefix. Brand trademarks belong to their respective owners.
