#!/usr/bin/env node
/**
 * 若不存在 src/config/oss-keys.local.ts，则从 example 复制，避免首次 clone 无法编译。
 */
import fs from "fs";
import path from "path";
import { fileURLToPath } from "url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const root = path.join(__dirname, "..");
const local = path.join(root, "src/config/oss-keys.local.ts");
const example = path.join(root, "src/config/oss-keys.local.example.ts");

if (!fs.existsSync(local)) {
  if (!fs.existsSync(example)) {
    console.error("ensure-oss-keys: missing", example);
    process.exit(1);
  }
  fs.copyFileSync(example, local);
  console.log("ensure-oss-keys: created src/config/oss-keys.local.ts from example (fill keys there)");
}
