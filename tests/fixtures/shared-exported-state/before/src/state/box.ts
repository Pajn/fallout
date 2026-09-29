class Box {
  value = 0;

  set(value: number) {
    this.value = value;
  }
}

export const box = new Box();

export function fill() {
  box.set(1);
}
