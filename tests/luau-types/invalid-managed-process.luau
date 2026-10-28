--!strict

local invalid_options = services.process {
    key = "query",
    shutdown_signal = "kill",
}

local query = services.process("query")

return ui.panel {
    children = {
        ui.button {
            on_click = function()
                query.start({ "program", 1 })
                query.stop("unexpected")
            end,
            children = {
                ui.text { text = query.running },
                ui.text { text = invalid_options.status:get() },
            },
        },
    },
}
