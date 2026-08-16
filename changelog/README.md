# changelog —— 版本日志与发布驱动

本目录是 **CI 发版的唯一数据源**(规则参照 `~/projects/CHAT/DSH/.github/workflows/rust.yml`)。

## 规则

- **何时自动构建发布**:向 `main` push 时,若本目录(`changelog/**`)
  或 `.github/workflows/**` 有变更 → 自动触发三平台构建 + Release 发布;
  平时改 `src/`、`README.md` 等**不触发**(想手动构建:GitHub → Actions → Run workflow)。
- **何时发布新版本**:发布流程读取 `changelog/` 下**最高版本号**文件
  (`vX.Y.Z.md`,按 semver 排序取最大),若 GitHub 上对应 Release 不存在 → 自动创建:
  - 版本号 = 文件名(`v0.1.0.md` → `v0.1.0`)
  - Release 说明 = 该文件内容
  - 产物 = 四份压缩包,重命名为 `localai-v<版本>-<平台>.*`
- **重复保护**:对应 Release 已存在 → 自动跳过,不会重复发版。

## 发版流程

```bash
# 1. 写新版本日志(这是唯一要做的)
cp changelog/v0.1.0.md changelog/v0.2.0.md   # 按实际版本号改
# 编辑内容...

# 2. 推送即触发构建 + 发布
git add changelog && git commit -m "release: v0.2.0" && git push
```

> 注意:changelog 中的版本应与 `Cargo.toml` 的 `version` 保持一致;
> 已发布过的版本文件不要改动,发布判断只看"是否存在对应 Release"。
