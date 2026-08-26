import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import './app.css'
import { App } from './App'
import { applyTheme, readTheme } from './theme'

const root = document.getElementById('root')
if (!root) throw new Error('#root missing from index.html')

// Before the first paint, not in an effect: a saved dark choice applied after
// mount is one white frame in a dark room.
applyTheme(readTheme())

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
