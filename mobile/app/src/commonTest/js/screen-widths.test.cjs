const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const context = { window: {} };
vm.runInNewContext(fs.readFileSync(path.join(__dirname, '../../androidMain/assets/terminal/screen-widths.js'), 'utf8'), context);
const Provider = context.window.Tty7ScreenWidths;

test('canonical Chinese, combining, modern emoji and VS16 retain server cell widths', () => {
    const provider = new Provider();
    provider.prepare([{ansi:'\x1b[0m\x1b[1G界\x1b[3Ge\u0301\x1b[4G🫠\x1b[6G❤\ufe0f\x1b[7G \x1b[0m'}], 7);
    assert.equal(provider.wcwidth('界'.codePointAt(0)), 2);
    assert.equal(provider.wcwidth('🫠'.codePointAt(0)), 2);
    assert.equal(provider.wcwidth('❤'.codePointAt(0)), 1);
    assert.equal(provider.wcwidth(0xfe0f), 0);
    assert.equal(provider.wcwidth(0x301), 0);
    assert.equal(provider.charProperties(0x301, provider.charProperties(101, 0)), 3);
    assert.equal(provider.charProperties(0xfe0f, provider.charProperties('❤'.codePointAt(0), 0)), 3);
});

test('last-column wide cell and colon underline styles use authoritative remaining width', () => {
    const provider = new Provider();
    provider.prepare([{ansi:'\x1b[0m\x1b[1G \x1b[2G\x1b[4:3m🫠\x1b[0m'}], 3);
    assert.equal(provider.wcwidth('🫠'.codePointAt(0)), 2);
    assert.equal(provider.wcwidth(32), 1);
});

test('replacement clears width metadata and invalid canonical positions are rejected', () => {
    const provider = new Provider();
    provider.prepare([{ansi:'\x1b[1G界'}], 2);
    provider.prepare([{ansi:'\x1b[1Ga\x1b[2Gb'}], 2);
    assert.equal(provider.wcwidth('界'.codePointAt(0)), 1);
    assert.throws(() => provider.prepare([{ansi:'\x1b[1Ga\x1b[4Gb'}], 4));
    assert.throws(() => provider.prepare([{ansi:'\x1b[1Ga\n'}], 1));
});

test('mixed text, emoji and edge-constrained presentations use their own cell width', () => {
    const provider = new Provider();
    provider.prepare([
        {row:0, ansi:'\x1b[1G❤\x1b[2G \x1b[3G❤️\x1b[5G '},
        {row:1, ansi:'\x1b[1Ga\x1b[2Gb\x1b[3Gc\x1b[4Gd\x1b[5G❤️'},
    ], 5);
    const heart = '❤'.codePointAt(0);
    assert.equal(provider.widthAt(0, 1, heart), 1);
    assert.equal(provider.widthAt(0, 3, heart), 2);
    assert.equal(provider.widthAt(1, 5, heart), 1);
    assert.equal(provider.widthAt(0, 3, 0xfe0f), 0);
});
