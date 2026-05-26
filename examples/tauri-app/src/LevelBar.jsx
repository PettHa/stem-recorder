import { useEffect, useRef } from 'react';

// Animated peak bar that reads from a ref + decays smoothly. Avoids React
// re-renders on every audio event (~20/s/channel) — the canvas updates from
// a single requestAnimationFrame loop instead.
export function LevelBar({ peakRef, colorVar, active }) {
  const canvasRef = useRef(null);
  const displayedRef = useRef(0);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return undefined;
    const ctx = canvas.getContext('2d');
    let raf;

    const draw = () => {
      const dpr = window.devicePixelRatio || 1;
      const cssW = canvas.clientWidth;
      const cssH = canvas.clientHeight;
      if (canvas.width !== Math.floor(cssW * dpr) || canvas.height !== Math.floor(cssH * dpr)) {
        canvas.width = Math.floor(cssW * dpr);
        canvas.height = Math.floor(cssH * dpr);
      }
      const w = canvas.width;
      const h = canvas.height;

      const target = active ? Math.min(1, peakRef.current ?? 0) : 0;
      // Fast attack, slow decay.
      const cur = displayedRef.current;
      const next = target > cur ? cur + (target - cur) * 0.55 : cur + (target - cur) * 0.08;
      displayedRef.current = next;

      // Track background
      const style = getComputedStyle(document.documentElement);
      const trackBg = style.getPropertyValue('--surface-sunk').trim() || '#e6e3d6';
      const fill = style.getPropertyValue(colorVar).trim() || '#888';
      const peakColor = style.getPropertyValue('--status-blocked').trim() || '#b5463a';
      const muted = style.getPropertyValue('--content-faint').trim() || '#aaa';

      ctx.clearRect(0, 0, w, h);
      ctx.fillStyle = trackBg;
      ctx.fillRect(0, 0, w, h);

      const fillW = Math.floor(w * next);
      ctx.fillStyle = active ? fill : muted;
      ctx.globalAlpha = active ? 0.95 : 0.4;
      ctx.fillRect(0, 0, fillW, h);
      ctx.globalAlpha = 1;

      // Clip warning when peak is hot (>0.95)
      if ((peakRef.current ?? 0) > 0.95) {
        ctx.fillStyle = peakColor;
        ctx.fillRect(w - Math.max(2, Math.floor(w * 0.02)), 0, Math.max(2, Math.floor(w * 0.02)), h);
      }

      // tick marks (10 / 20 / ... %)
      ctx.fillStyle = trackBg;
      for (let i = 1; i < 10; i++) {
        const x = Math.floor((w * i) / 10);
        ctx.fillRect(x, 0, 1, h);
      }

      raf = requestAnimationFrame(draw);
    };
    raf = requestAnimationFrame(draw);
    return () => cancelAnimationFrame(raf);
  }, [peakRef, colorVar, active]);

  return <canvas ref={canvasRef} className="level-bar" aria-hidden="true" />;
}
