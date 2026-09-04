// 主题状态(#72):localStorage 持久化 + prefers-color-scheme 兜底。
// data-theme 挂在 <html>;index.html 的预上色脚本在本模块加载前
// 已完成首次应用(防闪色),这里只负责读取当前值与切换。

export type Theme = "light" | "dark";

const STORAGE_KEY = "mf.theme";

export function preferredTheme(): Theme {
  const stored = localStorage.getItem(STORAGE_KEY);
  if (stored === "light" || stored === "dark") return stored;
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

export function currentTheme(): Theme {
  return document.documentElement.dataset.theme === "dark" ? "dark" : "light";
}

export function applyTheme(theme: Theme): void {
  document.documentElement.dataset.theme = theme;
}

export function toggleTheme(): Theme {
  const next: Theme = currentTheme() === "dark" ? "light" : "dark";
  localStorage.setItem(STORAGE_KEY, next);
  applyTheme(next);
  return next;
}
