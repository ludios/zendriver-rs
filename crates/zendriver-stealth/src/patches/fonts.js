(function (allow, seed) {
  __zdReplace(CanvasRenderingContext2D.prototype, 'measureText', (orig) => function (text) {
    const m = orig.call(this, text);
    // Sub-pixel width noise, keyed by the native metrics: the same text in
    // the same font measures the same in every read, context and page load.
    const rng = __zdKeyedRng(seed, [m.width, m.actualBoundingBoxLeft,
      m.actualBoundingBoxRight, m.actualBoundingBoxAscent, m.actualBoundingBoxDescent]);
    try { Object.defineProperty(m, 'width', { value: m.width + (rng() - 0.5) * 1e-3 }); }
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
