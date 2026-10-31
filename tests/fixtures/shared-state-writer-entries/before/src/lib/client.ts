type Sink = { send(event: string): void }

let instance: Sink | null = null

export function init(make: () => Sink) {
  instance = make()
}

export function track(event: string) {
  instance?.send(event)
}
