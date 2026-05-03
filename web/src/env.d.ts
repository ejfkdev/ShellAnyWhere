/// Vite-defined globals

// Compression Streams API (https://wicg.github.io/compression/)
interface DecompressionStream {
  readonly readable: ReadableStream<Uint8Array>;
  readonly writable: WritableStream<Uint8Array>;
}

declare var DecompressionStream: {
  new (format: "deflate-raw" | "deflate" | "gzip"): DecompressionStream;
  prototype: DecompressionStream;
};

declare module "*.css" {
  const content: string;
  export default content;
}

declare module "*.module.css" {
  const classes: Record<string, string>;
  export default classes;
}

declare module "eruda" {
  interface ErudaOptions {
    container?: HTMLElement | string;
    tool?: string[];
    inline?: boolean;
    autoScale?: boolean;
    useShadowDom?: boolean;
  }

  interface Eruda {
    init(options?: ErudaOptions): void;
    show(toolName?: string): void;
    hide(): void;
    _devTools?: { active: boolean };
  }

  const eruda: Eruda;
  export default eruda;
}
