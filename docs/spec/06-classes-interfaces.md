# 6. Classes & interfaces

## 6.1 Classes
```ts
class Animal {
  static count = 0
  static { Animal.count = 0 }                 // static init block

  constructor(public name: string, protected age: i32) { count++ }
  constructor(name: string) { this(name, 0) }  // overloaded constructor delegates

  public get description(): string { `${name} (${age})` }
  public set nickname(v: string) { … }

  public virtual speak(): string { "..." }
  public describe(): string { `${name} says ${speak()}` }   // not virtual, can't be overridden
}

class Dog extends Animal {
  constructor(name: string) { super(name, 3) }
  public override speak(): string { "Woof" }
}
```
- **Visibility**: class members are **private by default**. `private` means class-private. Use `public` or `protected` to widen
  them. Module-level declarations are private unless `export`ed.
- **Implicit members**: inside methods, `name` means `this.name` unless a local shadows it.
- **Inheritance**: single inheritance. Classes are **open** to extension by default, and
  `final class` prevents it.
- **Methods are final by default.** A method has to be `virtual` (or `abstract`) before it can be
  overridden, and an override has to say `override`. Leaving out `override` is an error.
- **Abstract**: `abstract class` and `abstract` methods, as in TS.
- **Constructors** can be overloaded. One constructor can delegate to another with
  `this(...)`. `super(...)` calls the base constructor. Constructor parameter properties
  (`public name: string`) declare and assign a field.
- Every field must be definitely assigned by the end of each constructor.

## 6.2 Interfaces
```ts
interface Shape {
  area(): f64
  perimeter(): f64
  describe(): string { `shape with area ${area():.2f}` }   // default method
}

class Circle implements Shape {
  constructor(public r: f64) {}
  public area(): f64 { Math.PI * r * r }
  public perimeter(): f64 { 2 * Math.PI * r }
}
```
- Interfaces are **nominal**. A type conforms only by declaring `implements` explicitly.
- Default methods take the place of mixins. If two interfaces provide conflicting defaults, the
  class has to override that method.
- Interface members are always public. A method that implements an interface member is **not**
  automatically virtual: subclasses can override it only if it's marked `virtual`.
- Interfaces can `extend` other interfaces and can be generic.
- Structs and enums can also `implement` interfaces.
- There are no `sealed` hierarchies. For a closed set of cases, use an `enum`.

## 6.3 Dispatch
- Calls to non-virtual methods and to struct methods are static.
- Calls to virtual methods go through a vtable.
- Calls through an interface type go through an interface table (fat pointer: object + itable).
