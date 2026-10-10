import { mkdir, copyFile, rm } from 'node:fs/promises';
/* keep the app, its Help and the website on the same version and the same "what's new" */
await import('./sync.mjs');
await rm('dist',{recursive:true,force:true});
await mkdir('dist',{recursive:true});
await copyFile('index.html','dist/index.html');
await copyFile('mini.html','dist/mini.html');
console.log('NOVA frontend ready.');
