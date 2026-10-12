'use strict';

// Synthesized rows position every canonical non-spacer cell with CHA. Derive
// widths from those positions, not the WebView's older Unicode width tables.
// This is a presentation provider, never a parser for the original PTY stream.
window.Tty7ScreenWidths = class {
    constructor() {
        this.version = 'tty7-canonical';
        this.widths = new Map();
        this.rows = new Map();
        this.row = 0;
        this.column = 1;
    }
    bind(target) {
        target.parser.registerCsiHandler({final:'H'}, params => {
            this.row = (params[0] || 1) - 1;
            this.column = params[1] || 1;
            return false;
        });
        target.parser.registerCsiHandler({final:'G'}, params => {
            this.column = params[0] || 1;
            return false;
        });
    }
    prepare(lines, cols) {
        this.widths.clear();
        this.rows.clear();
        this.row = 0;
        this.column = 1;
        for (let row = 0; row < lines.length; ++row) {
            const line = lines[row], columns = line.cols ?? cols;
            const positions = {bases:new Uint32Array(columns + 1), widths:new Uint8Array(columns + 1)};
            this.rows.set(line.row ?? row, positions);
            const cells = [...line.ansi.matchAll(/\x1b\[(\d+)G/g)];
            for (let i = 0; i < cells.length; ++i) {
                const start = Number(cells[i][1]);
                const next = i + 1 < cells.length ? Number(cells[i + 1][1]) : columns + 1;
                const end = i + 1 < cells.length ? cells[i + 1].index : line.ansi.length;
                const text = line.ansi.slice(cells[i].index + cells[i][0].length, end).replace(/\x1b\[[0-9;:]*m/g, '');
                const width = next - start;
                if (width < 1 || width > 2 || start < 1 || next > columns + 1 || !text || /[\x00-\x1f\x7f]/.test(text)) {
                    throw new Error('无效的规范终端单元格');
                }
                positions.bases[start] = text.codePointAt(0);
                positions.widths[start] = width;
                let base = true;
                for (const char of text) {
                    const code = char.codePointAt(0);
                    const cellWidth = base ? width : 0;
                    const known = this.widths.get(code);
                    // The same base may be narrow as text, wide with VS16,
                    // or constrained to one cell at the right edge.
                    this.widths.set(code, known === undefined ? cellWidth : Math.min(known, cellWidth));
                    base = false;
                }
            }
        }
    }
    widthAt(row, column, codepoint) {
        const positions = this.rows.get(row);
        if (positions?.bases[column] === codepoint) return positions.widths[column];
        return this.widths.get(codepoint) ?? (codepoint < 32 ? 0 : 1);
    }
    wcwidth(codepoint) { return this.widthAt(this.row, this.column, codepoint); }
    charProperties(codepoint, preceding) {
        let width = this.wcwidth(codepoint);
        const previousWidth = (preceding >> 1) & 3;
        const join = width === 0 && previousWidth !== 0;
        if (join) width = previousWidth;
        // Public IUnicodeVersionProvider encoding: width bits 1..2, join bit 0.
        return (width << 1) | (join ? 1 : 0);
    }
};
