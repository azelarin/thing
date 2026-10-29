--!strict

local invalid_options = services.process {
    key = "query",
    shutdown_signal = "kill",
}

local query = services.process("query")
local scheduled = services.process {
    key = "scheduled",
    command = { "program" },
    interval = 1000,
}

local invalid_schedule = services.process {
    key = "invalid-schedule",
    command = { "program", 1 },
}

return ui.panel {
    children = {
        ui.button {
            on_click = function()
                query.start({ "program", 1 })
                query.stop("unexpected")
                query.refresh()
                scheduled.start({ "program" })
                scheduled.cancel("unexpected")
            end,
            children = {
                ui.text { text = query.running },
                ui.text { text = invalid_options.status:get() },
                ui.text { text = invalid_schedule.status:get() },
            },
        },
    },
}
