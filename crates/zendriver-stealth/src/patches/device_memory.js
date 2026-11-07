// Patch deviceMemory on Navigator.prototype; no CDP command overrides it.
// Chrome defines deviceMemory only in secure contexts, so this patch doesn't
// define it anywhere else. `fp.memoryGb` is the memory size to present.
if ('deviceMemory' in Navigator.prototype) {
    __zdGetter(Navigator.prototype, 'deviceMemory', () => fp.memoryGb, { enumerable: true });
}
