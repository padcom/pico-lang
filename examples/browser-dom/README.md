# DOM demo (`js:dom`)

```sh
pico build --target wasm32-browser examples/browser-dom/app.pi -o examples/browser-dom/app
python3 -m http.server -d examples/browser-dom
```

`import { document, console } from "js:dom"` is typed from TypeScript's `lib.dom.d.ts`
(vendored in `third_party/typescript`): the compiler generates pico classes for the names imported
and the interfaces they use, so `document.getElementById("app")` is an `HTMLElement?` and
`button.textContent = …` sets the DOM property. Event listeners are pico closures wrapped with
`JsValue.fn`.
