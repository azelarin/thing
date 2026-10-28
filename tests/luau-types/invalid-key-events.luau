--!strict

return ui.panel {
    on_key = function(event)
        event.key = false
        return true
    end,
    children = { ui.popup {
        on_key = function(_event)
            return "handled"
        end,
    } },
}
