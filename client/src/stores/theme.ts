import { computed, ref } from "vue";
import { defineStore } from "pinia";

/** 主题偏好：亮色 / 暗色 / 跟随系统。 */
export type ThemeMode = "dark" | "light" | "system";

/** 主题偏好持久化键（localStorage，UI 进程本地存储）。 */
const STORAGE_KEY = "aria2-harness-theme";

const media =
  typeof window !== "undefined" && typeof window.matchMedia === "function"
    ? window.matchMedia("(prefers-color-scheme: dark)")
    : null;

function readInitialMode(): ThemeMode {
  try {
    const raw = localStorage.getItem(STORAGE_KEY);
    return raw === "dark" || raw === "system" ? raw : "light";
  } catch {
    // 受限环境（禁用 localStorage）下回退亮色（默认主题）。
    return "light";
  }
}

/** 同步 html 根节点的主题类名：Tailwind 的 dark: 变体依赖它。 */
export function applyThemeClass(mode: ThemeMode) {
  const dark = mode === "system" ? Boolean(media?.matches) : mode === "dark";
  document.documentElement.classList.toggle("dark", dark);
}

export const useThemeStore = defineStore("theme", () => {
  const mode = ref<ThemeMode>(readInitialMode());
  const systemDark = ref(Boolean(media?.matches));

  // 跟随系统时，真实响应系统配色变化（含 Windows 设置里切换深色模式）。
  media?.addEventListener?.("change", (event) => {
    systemDark.value = event.matches;
    if (mode.value === "system") applyThemeClass("system");
  });

  const isDark = computed(() =>
    mode.value === "system" ? systemDark.value : mode.value === "dark",
  );

  function setMode(next: ThemeMode) {
    mode.value = next;
    try {
      localStorage.setItem(STORAGE_KEY, next);
    } catch {
      // 持久化失败不影响本次切换。
    }
    applyThemeClass(next);
  }

  function toggle() {
    setMode(isDark.value ? "light" : "dark");
  }

  return { mode, isDark, setMode, toggle };
});
