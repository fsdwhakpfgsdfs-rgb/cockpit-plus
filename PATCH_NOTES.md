# 本地构建修改（非上游）

为了让桌面端在 16GB 内存机器上编译通过，改了两处：

1. `src-tauri/Cargo.toml`
   - `crate-type = ["staticlib", "rlib"]` → `["rlib"]`
   - staticlib 仅 iOS/Android 需要；桌面构建它会额外生成巨大的 .lib，是 OOM 主因
   - 若要打移动端包，改回原值

2. `Cargo.toml`
   - `[profile.dev] debug = "line-tables-only"` → `debug = 0`
   - 去掉调试信息，显著降低 rustc 峰值内存

3. `src-tauri/.cargo/config.toml`
   - `[build] incremental = false`

另外修复了上游一个语法错误：
`src-tauri/src/modules/codex_local_access_native_zcode.rs`
   - 第 157 行缺 `#[test] fn bigmodel_api_key_credential_headers_use_x_api_key() {` 函数头
   - 原因：PR #3282 合并时丢失，导致 `mod tests` 提前闭合、文件末尾多一个 `}`
