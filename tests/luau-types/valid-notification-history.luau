--!strict

local notifications = services.notifications
local draft = ui.state('')

local function card(record: ThingNotification): ThingWidget
    local children: { ThingWidget } = { ui.text { text = record.summary } }
    if record.image then
        table.insert(children, ui.image { source = record.image })
    end
    if record.closed_reason == 'expired' then
        table.insert(children, ui.text { text = 'missed' })
    end
    if record.can_reply then
        table.insert(children, ui.text_input {
            value = function() return draft:get() end,
            placeholder = record.reply_placeholder,
            on_change = function(value) draft:set(value) end,
            on_submit = function(value) notifications.reply(record.key, value) end,
        })
    end
    return ui.button {
        key = record.key,
        on_secondary_click = function()
            if record.closed_reason then
                notifications.forget(record.key)
            else
                notifications.dismiss(record.key)
            end
        end,
        children = children,
    }
end

return ui.panel {
    children = function()
        local cards = {}
        for _, record in notifications.history:get() do
            table.insert(cards, card(record))
        end
        table.insert(cards, ui.button {
            on_click = function() notifications.clear_history() end,
        })
        return cards
    end,
}
