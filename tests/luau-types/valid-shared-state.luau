--!strict

type Weather = { temperature: number, conditions: { string } }

local weather = services.shared_state {
    key = "weather",
    initial = { temperature = 18, conditions = {} } :: Weather,
}
local count = services.shared_state { key = "count", initial = 0 }

ui.command("weather", function(arguments)
    weather:set(arguments)
    return nil
end)

return ui.panel {
    children = {
        ui.button {
            on_click = function()
                count:set(count:get() + 1)
                local current: Weather = weather:get()
                weather:set({ temperature = current.temperature, conditions = { "rain" } })
            end,
            children = {
                ui.text {
                    text = function()
                        return `{weather:get().temperature} {count:get()}`
                    end,
                },
            },
        },
    },
}
