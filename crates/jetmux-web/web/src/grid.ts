// Draws a session view onto a canvas from the daemon's grid frames: the
// same View and Frame the terminal client composes, so the browser never
// parses VT. Canvas2d for now; anything implementing `GridRenderer` can
// replace it (WebGL).
import type { Color } from './proto/Color';
import type { Cursor } from './proto/Cursor';
import type { Frame } from './proto/Frame';
import type { Modes } from './proto/Modes';
import type { Rect } from './proto/Rect';
import type { Style } from './proto/Style';
import type { View } from './proto/View';
import type { WebTheme } from './proto/WebTheme';

export interface GridRenderer {
  setView(view: View): void;
  frame(pane: number, frame: Frame): void;
  /** Cell size multiplier: pinch zoom, or fitting a wide session to a phone. */
  setScale(scale: number): void;
  /** Cell size in CSS pixels at scale 1, for working out how many fit. */
  baseCell(): { w: number; h: number };
  /** The pane and cell at a point in CSS pixels on the canvas. */
  hit(x: number, y: number): { pane: number; col: number; row: number } | undefined;
  linkAt(x: number, y: number): string | undefined;
  /** A pane's visible text, for selecting it natively. */
  text(pane: number): string;
  modes(pane: number): Modes | undefined;
}

interface Cell {
  text: string;
  width: number;
  style: Style;
  link: string | undefined;
}

interface Grid {
  rows: Cell[][];
  cursor: Cursor | null;
  modes: Modes;
}

const FONT = "ui-monospace, SFMono-Regular, Menlo, Consolas, 'Liberation Mono', monospace";
const BASE_FONT_PX = 13;
const BORDER = '#555';
const BORDER_FOCUSED = '#7fbf6a';

function color(c: Color, fallback: string, palette: string[]): string {
  if (c === 'Default') return fallback;
  if ('Palette' in c) return palette[c.Palette] ?? fallback;
  const [r, g, b] = c.Rgb;
  return `rgb(${r},${g},${b})`;
}

const PLAIN: Style = {
  fg: 'Default',
  bg: 'Default',
  underline_color: 'Default',
  underline: 'None',
  bold: false,
  faint: false,
  italic: false,
  blink: false,
  inverse: false,
  invisible: false,
  strikethrough: false,
  overline: false,
};

export class CanvasGrid implements GridRenderer {
  private ctx: CanvasRenderingContext2D;
  private view: View | undefined;
  private grids = new Map<number, Grid>();
  private scale = 1;
  private cellW = 8;
  private cellH = 16;
  private fontPx = BASE_FONT_PX;
  private dirty = new Map<number, Set<number> | 'all'>();
  private drawAll = true;
  private queued = false;

  /** jet's theme, from the server. */
  constructor(
    private canvas: HTMLCanvasElement,
    private theme: WebTheme,
  ) {
    const ctx = canvas.getContext('2d', { alpha: false });
    if (!ctx) throw new Error('no 2d canvas');
    this.ctx = ctx;
    this.measure();
  }

  baseCell() {
    this.ctx.font = `${BASE_FONT_PX}px ${FONT}`;
    return { w: this.ctx.measureText('M').width, h: Math.ceil(BASE_FONT_PX * 1.3) };
  }

  private measure() {
    this.fontPx = BASE_FONT_PX * this.scale;
    this.ctx.font = `${this.fontPx}px ${FONT}`;
    this.cellW = this.ctx.measureText('M').width;
    this.cellH = Math.ceil(this.fontPx * 1.3);
  }

  setScale(scale: number) {
    if (Math.abs(scale - this.scale) < 0.001) return;
    this.scale = scale;
    this.measure();
    this.resizeCanvas();
    this.queue(true);
  }

  setView(view: View) {
    this.view = view;
    for (const id of [...this.grids.keys()]) {
      if (!view.panes.some((p) => p.id === id)) this.grids.delete(id);
    }
    this.resizeCanvas();
    this.queue(true);
  }

  frame(pane: number, frame: Frame) {
    let grid = this.grids.get(pane);
    if (!grid) {
      grid = { rows: [], cursor: null, modes: frame.modes };
      this.grids.set(pane, grid);
    }
    grid.rows.length = frame.size.rows;
    grid.cursor = frame.cursor;
    grid.modes = frame.modes;
    const changed = new Set<number>();
    for (const row of frame.rows) {
      const cells: Cell[] = [];
      for (const run of row.runs) {
        const style = frame.styles[run.style] ?? PLAIN;
        const link = run.link > 0 ? frame.links[run.link - 1] : undefined;
        if (run.cluster !== null) {
          cells.push({ text: run.text, width: run.cluster, style, link });
        } else {
          for (const ch of run.text) cells.push({ text: ch, width: 1, style, link });
        }
      }
      grid.rows[row.y] = cells;
      changed.add(row.y);
    }
    // The cursor moving needs its old and new rows redrawn: redraw the
    // pane when no rows changed but the cursor did.
    const prev = this.dirty.get(pane);
    if (frame.full || prev === 'all') this.dirty.set(pane, 'all');
    else this.dirty.set(pane, new Set([...(prev ?? []), ...changed]));
    if (frame.rows.length === 0) this.dirty.set(pane, 'all');
    this.queue(false);
  }

  modes(pane: number) {
    return this.grids.get(pane)?.modes;
  }

  private resizeCanvas() {
    const area = this.view?.area;
    if (!area) return;
    const dpr = window.devicePixelRatio || 1;
    const w = Math.ceil(area.cols * this.cellW);
    const h = area.rows * this.cellH;
    this.canvas.style.width = `${w}px`;
    this.canvas.style.height = `${h}px`;
    this.canvas.width = Math.ceil(w * dpr);
    this.canvas.height = Math.ceil(h * dpr);
    this.ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
  }

  private queue(all: boolean) {
    this.drawAll ||= all;
    if (this.queued) return;
    this.queued = true;
    requestAnimationFrame(() => {
      this.queued = false;
      this.draw();
    });
  }

  private draw() {
    const view = this.view;
    if (!view) return;
    const ctx = this.ctx;
    ctx.font = `${this.fontPx}px ${FONT}`;
    ctx.textBaseline = 'middle';
    if (this.drawAll) {
      this.drawAll = false;
      this.dirty.clear();
      ctx.fillStyle = this.theme.background;
      ctx.fillRect(0, 0, view.area.cols * this.cellW, view.area.rows * this.cellH);
      this.drawBorders(view);
      for (const pane of view.panes) this.drawPane(pane.id, pane.rect, 'all', pane.focused);
      return;
    }
    for (const [id, rows] of this.dirty) {
      const pane = view.panes.find((p) => p.id === id);
      if (pane) this.drawPane(id, pane.rect, rows, pane.focused);
    }
    this.dirty.clear();
  }

  private drawBorders(view: View) {
    const ctx = this.ctx;
    const inPane = (x: number, y: number) =>
      view.panes.some(
        (p) => x >= p.rect.x && x < p.rect.x + p.rect.cols && y >= p.rect.y && y < p.rect.y + p.rect.rows,
      );
    const focused = view.panes.find((p) => p.focused)?.rect;
    const near = (r: Rect | undefined, x: number, y: number) =>
      !!r && x >= r.x - 1 && x <= r.x + r.cols && y >= r.y - 1 && y <= r.y + r.rows;
    const { cellW: w, cellH: h } = this;
    for (let y = 0; y < view.area.rows; y++) {
      for (let x = 0; x < view.area.cols; x++) {
        if (inPane(x, y)) continue;
        ctx.fillStyle = near(focused, x, y) ? BORDER_FOCUSED : BORDER;
        const vertical = inPane(x - 1, y) || inPane(x + 1, y);
        if (vertical) ctx.fillRect(x * w + w / 2 - 0.5, y * h, 1, h);
        else ctx.fillRect(x * w, y * h + h / 2 - 0.5, w, 1);
      }
    }
  }

  private drawPane(id: number, rect: Rect, rows: Set<number> | 'all', focused: boolean) {
    const grid = this.grids.get(id);
    const ctx = this.ctx;
    const { cellW: w, cellH: h } = this;
    for (let y = 0; y < rect.rows; y++) {
      if (rows !== 'all' && !rows.has(y) && !(grid?.cursor && grid.cursor.y === y)) continue;
      const top = (rect.y + y) * h;
      ctx.fillStyle = this.theme.background;
      ctx.fillRect(rect.x * w, top, rect.cols * w, h);
      let x = 0;
      for (const cell of grid?.rows[y] ?? []) {
        if (x >= rect.cols) break;
        const width = Math.min(cell.width, rect.cols - x);
        const s = cell.style;
        const { foreground, background, palette } = this.theme;
        let fg = color(s.fg, foreground, palette);
        let bg = color(s.bg, background, palette);
        if (s.inverse) [fg, bg] = [bg, fg];
        const left = (rect.x + x) * w;
        if (bg !== background) {
          ctx.fillStyle = bg;
          ctx.fillRect(left, top, width * w, h);
        }
        if (cell.text !== ' ' && !s.invisible) {
          ctx.font = `${s.italic ? 'italic ' : ''}${s.bold ? 'bold ' : ''}${this.fontPx}px ${FONT}`;
          ctx.globalAlpha = s.faint ? 0.6 : 1;
          ctx.fillStyle = fg;
          ctx.fillText(cell.text, left, top + h / 2);
          ctx.globalAlpha = 1;
        }
        if (s.underline !== 'None' || cell.link) {
          ctx.fillStyle = s.underline_color === 'Default' ? fg : color(s.underline_color, fg, palette);
          ctx.fillRect(left, top + h - 2, width * w, 1);
        }
        if (s.strikethrough) {
          ctx.fillStyle = fg;
          ctx.fillRect(left, top + h / 2, width * w, 1);
        }
        x += cell.width;
      }
      const cursor = grid?.cursor;
      if (focused && cursor && cursor.y === y && cursor.x < rect.cols) {
        const left = (rect.x + cursor.x) * w;
        ctx.fillStyle = this.theme.cursor ?? this.theme.foreground;
        ctx.globalAlpha = 0.6;
        if (cursor.shape === 'Bar') ctx.fillRect(left, top, 2, h);
        else if (cursor.shape === 'Underline') ctx.fillRect(left, top + h - 2, w, 2);
        else if (cursor.shape === 'BlockHollow') {
          ctx.strokeStyle = ctx.fillStyle;
          ctx.strokeRect(left + 0.5, top + 0.5, w - 1, h - 1);
        } else ctx.fillRect(left, top, w, h);
        ctx.globalAlpha = 1;
      }
    }
  }

  hit(px: number, py: number) {
    const col = Math.floor(px / this.cellW);
    const row = Math.floor(py / this.cellH);
    const pane = this.view?.panes.find(
      (p) => col >= p.rect.x && col < p.rect.x + p.rect.cols && row >= p.rect.y && row < p.rect.y + p.rect.rows,
    );
    return pane ? { pane: pane.id, col: col - pane.rect.x, row: row - pane.rect.y } : undefined;
  }

  linkAt(px: number, py: number) {
    const at = this.hit(px, py);
    if (!at) return undefined;
    let x = 0;
    for (const cell of this.grids.get(at.pane)?.rows[at.row] ?? []) {
      if (at.col < x + cell.width) return cell.link;
      x += cell.width;
    }
    return undefined;
  }

  text(pane: number) {
    return (this.grids.get(pane)?.rows ?? [])
      .map((row) =>
        (row ?? [])
          .map((c) => c.text)
          .join('')
          .trimEnd(),
      )
      .join('\n')
      .replace(/\n+$/, '');
  }
}
