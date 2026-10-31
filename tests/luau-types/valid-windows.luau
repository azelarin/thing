--!strict

local open = ui.state(false)
local settings = ui.window_state("settings")

return {
    ui.panel {
        key = "bar",
        children = {
            ui.button {
                on_click = function()
                    open:set(true)
                end,
                children = { ui.text { text = "settings" } },
            },
        },
    },
    ui.window {
        key = "settings",
        title = function(): string
            return if settings.maximized:get() then "settings (maximized)" else "settings"
        end,
        app_id = "org.example.settings",
        size = { width = 480, height = 360 },
        min_size = { width = 320, height = 240 },
        max_size = nil,
        visible = function(): boolean
            return open:get()
        end,
        on_close = function()
            open:set(false)
        end,
        children = {
            ui.column {
                children = {
                    ui.text {
                        text = function(): string
                            local width: number = settings.width:get()
                            local activated: boolean = settings.activated:get()
                            return `{width} {activated} {settings.decorated:get()}`
                        end,
                    },
                    ui.button {
                        enabled = function(): boolean
                            return settings.can_fullscreen:get()
                        end,
                        on_click = function()
                            settings.set_fullscreen(not settings.fullscreen:get())
                        end,
                    },
                },
            },
        },
    },
}
