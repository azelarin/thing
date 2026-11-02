--!strict

local count = services.shared_state { key = "count", initial = 0 }

return ui.panel {
    children = {
        ui.button {
            on_click = function()
                count:get(1)
                count:set("wrong")
                count.value = 2
            end,
        },
    },
}
