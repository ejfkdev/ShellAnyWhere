/// Lazy-loaded eruda — dynamically imported as a separate Vite chunk.
/// Our DBG button toggles the panel. Not using inline mode because
/// inline forces displaySize to 100% and disables the resize handle.

let erudaInstance: any = null;

export async function loadEruda(): Promise<void> {
  if (erudaInstance) {
    if (erudaInstance._devTools?.active) {
      erudaInstance.hide();
    } else {
      erudaInstance.show();
    }
    return;
  }

  const mod = await import("eruda");
  const eruda = mod.default || mod;
  eruda.init({
    defaults: {
      displaySize: 50,
      transparency: 0.95,
    },
  });

  eruda.show();
  erudaInstance = eruda;
}
