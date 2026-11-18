import { render } from 'solid-js/web';
import { App } from './App';
import './style.css';

if ('serviceWorker' in navigator) {
  navigator.serviceWorker.register('/sw.js').catch(() => {});
}

const root = document.getElementById('root');
if (root) render(() => <App />, root);
