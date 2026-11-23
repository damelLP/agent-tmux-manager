import { defineConfig } from 'vite';
import { createFake } from './dev/fake.ts';
import { attachFake, diagnostics } from './dev/server.mjs';

export default defineConfig({
  server:{host:'127.0.0.1',hmr:false},
  build:{assetsInlineLimit:0,modulePreload:false},
  plugins:[{
    name:'atm-synthetic-chat',
    configureServer(server) {
      if (server.config.server.host !== '127.0.0.1') throw new Error('The synthetic chat server is loopback only');
      const http=server.httpServer;
      if (!http) return;
      const close=attachFake(http,createFake());
      http.on('close',close);
      server.middlewares.use((req,res,next)=>{if(!diagnostics(req,res)) next();});
    },
  }],
});
