/**
 * OSS 凭证（仅内部使用）
 *
 * 首次 `npm run dev` / `npm run build` 会从本文件自动生成 `oss-keys.local.ts`（已 .gitignore）。
 * 在 `oss-keys.local.ts` 中填写 AccessKeyId 与 Secret 即可。
 * 上传路径固定为 xuntian-pro-public / oss-cn-beijing / 前缀 xuntian/map（界面不可改，后端亦强制）。
 * 亦可手动：`cp src/config/oss-keys.local.example.ts src/config/oss-keys.local.ts`
 *
 * 后端仍支持环境变量 OSS_ACCESS_KEY_ID / OSS_ACCESS_KEY_SECRET（未在请求里传时生效）。
 */
export const OSS_ACCESS_KEY_ID = "";
export const OSS_ACCESS_KEY_SECRET = "";
