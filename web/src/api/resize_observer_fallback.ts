// ResizeObserver 轮询兜底(#97):ZCode IAB 等嵌入 webview 不派发 RO
// 回调(连 observe 后的初始通知也没有),React Flow 依赖它做节点测量与
// handleBounds 计算——失效时 DAG 编辑器只显示节点、永不画边;xterm 的
// fit 同理。启动期检测:原生失效则替换为 getBoundingClientRect 轮询实现。
//
// 语义对齐:observe 后异步触发一次初始回调(原生行为,库依赖它做首次
// 测量);此后每 200ms 比对矩形,变化才通知;异常不中断(与原生一致)。

interface RectLike extends DOMRectReadOnly {
  x: number;
  y: number;
  width: number;
  height: number;
  top: number;
  right: number;
  bottom: number;
  left: number;
}

function rectOf(el: Element): RectLike {
  const r = el.getBoundingClientRect();
  return {
    x: r.x,
    y: r.y,
    width: r.width,
    height: r.height,
    top: r.top,
    right: r.right,
    bottom: r.bottom,
    left: r.left,
    toJSON: () => ({ ...r }),
  } as RectLike;
}

class PollingResizeObserver implements ResizeObserver {
  private callback: ResizeObserverCallback;
  private targets = new Map<Element, RectLike>();
  private timer: ReturnType<typeof setInterval> | null = null;

  constructor(callback: ResizeObserverCallback) {
    this.callback = callback;
  }

  observe(target: Element): void {
    this.targets.set(target, rectOf(target));
    // 原生语义:observe 后异步初始通知一次
    setTimeout(() => {
      if (this.targets.has(target)) {
        this.notify(target);
      }
    }, 0);
    this.start();
  }

  unobserve(target: Element): void {
    this.targets.delete(target);
    if (this.targets.size === 0) this.stop();
  }

  disconnect(): void {
    this.targets.clear();
    this.stop();
  }

  private notify(target: Element): void {
    const rect = rectOf(target);
    try {
      this.callback(
        [
          {
            target,
            contentRect: rect,
            borderBoxSize: [{ inlineSize: rect.width, blockSize: rect.height }],
            contentBoxSize: [{ inlineSize: rect.width, blockSize: rect.height }],
            devicePixelContentBoxSize: [
              { inlineSize: rect.width * window.devicePixelRatio, blockSize: rect.height * window.devicePixelRatio },
            ],
          },
        ],
        this,
      );
    } catch {
      /* 与原生一致:观察者异常不中断轮询 */
    }
  }

  private start(): void {
    if (this.timer !== null) return;
    this.timer = setInterval(() => {
      for (const [target, prev] of this.targets) {
        const now = rectOf(target);
        if (
          now.width !== prev.width ||
          now.height !== prev.height ||
          now.x !== prev.x ||
          now.y !== prev.y
        ) {
          this.targets.set(target, now);
          this.notify(target);
        }
      }
    }, 200);
  }

  private stop(): void {
    if (this.timer !== null) {
      clearInterval(this.timer);
      this.timer = null;
    }
  }
}

/**
 * 启动期检测并按需替换。用 setTimeout(而非 rAF——不可见 webview 中
 * rAF 永不派发,会把 bootstrap 卡成白屏)判定:原生 RO 回调在布局后
 * 异步派发,150ms 内未收到即视为失效。返回 true 表示原生可用。
 */
export function installResizeObserverFallback(): Promise<boolean> {
  return new Promise((resolve) => {
    const probe = document.createElement("div");
    probe.style.cssText = "width:10px;height:10px;position:fixed;left:-100px;top:0";
    document.body.appendChild(probe);
    let fired = false;
    try {
      const ro = new ResizeObserver(() => {
        fired = true;
      });
      ro.observe(probe);
      probe.style.width = "44px";
    } catch {
      fired = false;
    }
    setTimeout(() => {
      probe.remove();
      if (!fired) {
        window.ResizeObserver = PollingResizeObserver as unknown as typeof ResizeObserver;
      }
      resolve(fired);
    }, 150);
  });
}
