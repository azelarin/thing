--!strict

local missing_reason = services.sleep_inhibitor { key = "download" }
local inhibitor = services.sleep_inhibitor { key = "download", reason = "running" }
local away = services.idle.watch { timeout = "5m" }
local callbacks = services.idle.watch { timeout = 1000, on_idle = true }

return ui.panel {
    children = {
        ui.button {
            on_click = function()
                inhibitor.start("now")
                inhibitor.state:set("active")
                away.idle:set(true)
            end,
            children = {
                ui.text { text = inhibitor.state },
                ui.text { text = function() return callbacks.idle:get() end },
                ui.text { text = missing_reason.error:get() },
            },
        },
    },
}
