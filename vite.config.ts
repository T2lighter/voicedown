import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// https://vitejs.dev/config/
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 5176,
    strictPort: true,
    // Vite 启动后立即预转换入口模块，避免首次请求时逐个编译
    warmup: {
      clientFiles: ["./src/main.tsx", "./src/App.tsx", "./src/App.css"],
    },
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    chunkSizeWarningLimit: 1024,
  },
  // 依赖预打包：Vite 启动时就用 esbuild 打包好，不等到首次 HTTP 请求
  optimizeDeps: {
    include: ["react", "react-dom"],
  },
});
