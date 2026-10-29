--!strict

local download = services.sleep_inhibitor {
    key = "download",
    reason = "a download is running",
}

local status: ThingSleepInhibitorStatus = download.state:get()
local error_text: string = download.error:get()

local away = services.idle.watch {
    timeout = 5 * 60 * 1000,
    on_idle = function(watch: ThingIdleWatch)
        local idle: boolean = watch.idle:get()
        if idle and services.output.name:get() == "DP-1" then
            download.stop()
        end
    end,
    on_resume = function() end,
}
services.idle.watch { timeout = 1000 }

return ui.panel {
    children = {
        ui.button {
            on_click = function()
                download.start()
            end,
            children = {
                ui.text {
                    text = function()
                        local available: boolean = services.idle.watch_available:get()
                        return `{status} {error_text} {away.idle:get()} {available}`
                    end,
                },
            },
        },
    },
}
