--!strict

local dropped = ui.state("")
local hovering = ui.state(false)

local target = ui.pointer_area {
    drop_types = { "files", "text" },
    on_drag_over = function(event: ThingDragOverEvent): ThingDragAction?
        hovering:set(event.phase == "enter" or event.phase == "move")
        if event.phase == "leave" or event.phase == "cancel" or #event.types == 0 then
            return nil
        end
        local position = event.x + event.y + event.surface_x + event.surface_y
        if position < 0 then
            return nil
        end
        return if table.find(event.actions, "move") then "move" else "copy"
    end,
    on_drop = function(event: ThingDropEvent)
        local kind: ThingDropType = event.type
        local action: ThingDragAction = event.action
        if event.files then
            dropped:set(kind .. action .. table.concat(event.files, "\n"))
        else
            dropped:set(event.text or "")
        end
        return true
    end,
    children = { ui.text { text = function() return dropped:get() end } },
}

local source = ui.pointer_area {
    on_pointer = function(event)
        if event.phase == "move" then
            event:start_drag {
                files = { "/tmp/example", "file:///tmp/other" },
                actions = { "copy", "move" },
                on_end = function(result: ThingDragResult)
                    if result == "move" then
                        dropped:set("")
                    end
                end,
            }
        elseif event.phase == "press" and event.shift then
            event:start_drag { text = "dragged text" }
        end
        return true
    end,
}

return ui.panel {
    children = { target, source },
}
