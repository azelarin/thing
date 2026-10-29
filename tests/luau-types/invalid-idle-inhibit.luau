--!strict

ui.panel { idle_inhibit = 1 }
ui.popup { idle_inhibit = "yes" }
local lock: ThingLockProperties = {}
lock.idle_inhibit = true
ui.lock(lock)

local available: number = services.idle.inhibit_available:get()
print(available)
