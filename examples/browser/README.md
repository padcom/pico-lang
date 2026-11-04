# Browser demo (wasm32-browser)

```sh
pico build --target wasm32-browser examples/browser/app.pi -o examples/browser/app
python3 -m http.server -d examples/browser
```

Open <http://localhost:8000>: the page imports the generated glue `app.js`, which loads
`app.wasm`; `app.d.ts` declares the exported API. The pico code keeps the state (a struct copied
to and from a plain object) and a `History` (a class, held by JavaScript as a handle), renders
HTML, and prints to the console (`println`, and a timer on the browser's event loop).
