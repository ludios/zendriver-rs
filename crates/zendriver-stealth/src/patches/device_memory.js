// Patch deviceMemory on Navigator.prototype; no CDP command overrides it.
// `fp` is the serialized Fingerprint object passed by the bundle factory.
__zdGetter(Navigator.prototype, 'deviceMemory', () => fp.memoryGb, { enumerable: true });
