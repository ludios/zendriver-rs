(function (allow, seed) {
  __zdReplace(CanvasRenderingContext2D.prototype, 'measureText', (orig) => function () {
    const m = orig.apply(this, arguments);
    const width = m.width;
    // No text, so nothing to perturb: with noise, its width would be
    // negative as often as not.
    if (width === 0) return m;
    // Sub-pixel width noise, keyed by the native width alone: every read of
    // a text agrees, whatever the context's alignment and baseline, and
    // texts as wide as each other stay so.
    const rng = __zdKeyedRng(seed, [width]);
    try { Object.defineProperty(m, 'width', { value: width + (rng() - 0.5) * 1e-3 }); }
    catch (e) {}
    return m;
  });
  // If an allow-list is provided, hide other fonts from FontFaceSet checks.
  if (Array.isArray(allow) && document.fonts && document.fonts.check) {
    __zdReplace(document.fonts, 'check', (origCheck) => function (font, text) {
      const fam = (font || '').split(/\s+/).pop();
      if (fam && allow.indexOf(fam.replace(/["']/g, '')) === -1) return false;
      return origCheck.call(document.fonts, font, text);
    });
  }
})(FONT_ALLOW, SEED);
