--!strict

services.notifications.set_expiry_paused(1, true)
services.notifications.set_expiry_paused('1', 'true')
local remaining: number = services.notifications.remaining('1')
return ui.panel {}
