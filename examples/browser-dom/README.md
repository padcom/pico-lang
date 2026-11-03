# DOM demo (`js:dom`)

```sh
ppl build --target wasm32-browser examples/browser-dom/app.ppl -o examples/browser-dom/app
python3 -m http.server -d examples/browser-dom
```

`import { document, console } from "js:dom"` is typed from TypeScript's `lib.dom.d.ts`
(vendored in `third_party/typescript`): the compiler generates ppl classes for the names imported
and the interfaces they use, so `document.getElementById("app")` is an `HTMLElement?` and
`button.textContent = …` sets the DOM property. Event listeners are ppl closures wrapped with
`JsValue.fn`.
