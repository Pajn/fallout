import { init } from './lib/client'
import { session } from './lib/session'
import { pages } from './gallery'

init(() => ({ send: console.log }))
session.start()
pages.forEach((page) => page())
