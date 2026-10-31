import { isReady } from '../lib/session'

export const ReadyPage = () => (isReady() ? 'ready' : 'waiting')
