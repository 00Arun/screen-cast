'use strict';
const { invoke } = window.__TAURI__.core;
invoke('about_info').then((i) => { document.getElementById('version').textContent = `Version ${i.version}`; });
// Opens inside Screen Cast's own sandboxed browser.
document.getElementById('site').addEventListener('click', () => invoke('open_developer_site'));
