/**
 * aria2 错误码 → 中文可读文案。
 *
 * 目的：前端绝不把引擎返回的英文原文（如 `Could not resolve host name`）直接甩给用户；
 * 统一按错误码翻译，未知码给出可读兜底。
 *
 * 码表来源：aria2 手册「EXIT CODES」章节 + 本项目新增的扩展码（34/35/36）。
 */
const ERROR_CODES: Record<number, string> = {
  1: "连接被拒绝 / 网络异常",
  2: "请求超时",
  3: "资源不存在（404）",
  4: "资源不存在（文件已被删除）",
  5: "下载速度过慢，已放弃",
  6: "网络问题（连接中断）",
  7: "存在未完成的下载",
  8: "服务器不支持断点续传",
  9: "磁盘空间不足",
  10: "分片长度与已有文件不一致",
  11: "重复下载",
  12: "种子文件损坏",
  13: "文件已存在",
  14: "重命名失败",
  15: "无法打开已有文件",
  16: "无法创建或截断文件",
  17: "文件系统错误",
  18: "无法创建目录",
  19: "DNS 解析失败",
  20: "Metalink 解析失败",
  21: "FTP 命令失败",
  22: "HTTP 响应头异常",
  23: "重定向次数过多",
  24: "HTTP 认证失败",
  25: "Bencode 解析失败",
  26: "种子文件缺少关键信息",
  27: "磁力链接格式无效",
  28: "选项或参数无效",
  29: "服务器不支持分段请求",
  30: "JSON-RPC 请求解析失败",
  32: "校验和不匹配",
  33: "CPU 或磁盘 IO 过载",
  34: "磁盘空间不足（预分配被拒绝）",
  35: "连接被重置",
  36: "网络被阻断（连续异常，已熔断）",
};

/** 把错误码翻译成中文；`0`/空值不视为错误。 */
export function translateErrorCode(code: string | number | undefined | null): string {
  if (code === undefined || code === null || code === "" || code === 0 || code === "0") {
    return "";
  }
  const numeric = typeof code === "number" ? code : Number(code);
  return ERROR_CODES[numeric] ?? `未知错误（code ${code}）`;
}

/** 是否为真实失败：必须是 error 状态、且错误码非 0 / 非空。 */
export function isRealFailure(status: string | undefined, code: string | undefined): boolean {
  return status === "error" && code !== undefined && code !== "" && code !== "0";
}
