use std::{
    collections::HashMap,
    fs::File,
    io::{self, Read},
    ops::Range,
    sync::{
        mpsc::{sync_channel, SyncSender},
        Arc, Condvar, Mutex, RwLock,
    },
    thread,
    time::Duration,
};

use ethercat::{
    AlState, DomainIdx, Master, MasterAccess, Offset, PdoCfg, PdoEntryIdx, PdoEntryInfo,
    PdoEntryPos, SlaveAddr, SlaveId, SlavePos, SmCfg,SmIdx, PdoPos, PdoIdx
};
use ethercat_esi::EtherCatInfo;

#[derive(Debug)]
pub struct EtherCatController {
    offsets: SlaveOffsets,
    slave_names: SlaveNames,

    data_lock: Arc<RwLock<Option<Vec<u8>>>>,
    ready_condvar: Arc<(Mutex<bool>, Condvar)>,
    cycle_condvar: Arc<(Mutex<bool>, Condvar)>,

    cmd_buff: SyncSender<(Range<usize>, Vec<u8>)>,
}

impl EtherCatController {
    pub fn open(
        master_id: u32,
        cycle_period: Duration,
    ) -> Result<Self, io::Error> {
        let (mut master, domain_idx, offsets, slave_names) = init_master(master_id)?;

        master.activate()?;

        for (s, o) in &offsets {
            log::debug!("PDO offsets of Slave {}:", u16::from(*s));
            for (name, pdos) in o {
                for (pdo, bit_len, offset) in pdos {
                    log::debug!(
                        " - \"{}\" : {:X}:{:X} - {:?}, bit length: {}",
                        name,
                        u16::from(pdo.idx),
                        u8::from(pdo.sub_idx),
                        offset,
                        bit_len
                    );
                }
            }
        }

        let data_lock = Arc::new(RwLock::new(None));
        let write_data_lock = Arc::clone(&data_lock);

        let ready_condvar = Arc::new((Mutex::new(false), Condvar::new()));
        let write_ready_condvar = Arc::clone(&ready_condvar);

        let cycle_condvar = Arc::new((Mutex::new(false), Condvar::new()));
        let write_cycle_condvar = Arc::clone(&cycle_condvar);

        let (tx, rx) = sync_channel::<(Range<usize>, Vec<u8>)>(5);

        let mut is_ready = false;

        thread::spawn(move || loop {
            master.receive().unwrap();
            master.domain(domain_idx).process().unwrap();
            master.domain(domain_idx).queue().unwrap();

            let data = master.domain_data(domain_idx).unwrap();

            log::debug!("{:?}", &data);

            if let Ok(mut write_guard) = write_data_lock.write() {
                *write_guard = Some(data.to_vec());
            }

            {
                let (lock, cvar) = &*write_cycle_condvar;
                let mut next_cycle = lock.lock().unwrap();
                *next_cycle = true;
                cvar.notify_one();
            }

            while let Ok((reg_addr_range, value)) = rx.try_recv() {
                data[reg_addr_range].copy_from_slice(&value);
            }

            master.send().unwrap();

            if !is_ready {
                let m_state = master.state().unwrap();
                log::debug!("Current state {:?}", m_state);

                if m_state.link_up && m_state.al_states == 8 {
                    let (lock, cvar) = &*write_ready_condvar;
                    let mut ready = lock.lock().unwrap();
                    *ready = true;
                    cvar.notify_one();
                    is_ready = true;

                    log::info!("Master ready!");
                }
            }

            thread::sleep(cycle_period);
        });

        Ok(EtherCatController {
            offsets,
            slave_names,
            data_lock,
            ready_condvar,
            cycle_condvar,
            cmd_buff: tx,
        })
    }

    pub fn get_slave_ids(&self) -> Vec<u16> {
        let mut ids: Vec<u16> = self
            .offsets
            .keys()
            .map(|slave_pos| u16::from(*slave_pos))
            .collect();
        ids.sort();
        ids
    }

    pub fn get_pdo_register(
        &self,
        slave_id: u16,
        register: &String,
        index: usize,
    ) -> Option<Vec<u8>> {
        let reg_addr_range = self.get_reg_addr_range(slave_id, register, index);

        (*self.data_lock.read().unwrap())
            .as_ref()
            .map(|data| data[reg_addr_range].to_vec())
    }

    pub fn set_pdo_register(&self, slave_id: u16, register: &String, index: usize, value: Vec<u8>) {
        let reg_addr_range = self.get_reg_addr_range(slave_id, register, index);

        self.cmd_buff.send((reg_addr_range, value)).unwrap();
    }

    pub fn get_pdo_registers(&self, slave_id: u16, register: &String) -> Option<Vec<Vec<u8>>> {
        let reg_addr_ranges = self.get_reg_addr_ranges(slave_id, register);

        let vals = reg_addr_ranges
            .iter()
            .map(|reg_addr_range| {
                (*self.data_lock.read().unwrap())
                    .as_ref()
                    .map(|data| data[reg_addr_range.clone()].to_vec())
            })
            .collect::<Option<Vec<Vec<u8>>>>()?;
        Some(vals)
    }

    pub fn set_pdo_registers(&self, slave_id: u16, register: &String, values: Vec<Vec<u8>>) {
        let reg_addr_ranges = self.get_reg_addr_ranges(slave_id, register);

        if values.len() != reg_addr_ranges.len() {
            // log::error!("values: {:?}", values);
            log::warn!(
                "Values length does not match register count, using first {} elements!",
                reg_addr_ranges.len()
            );
        }

        for (reg_addr_range, v) in reg_addr_ranges.iter().zip(values) {
            self.cmd_buff.send((reg_addr_range.clone(), v)).unwrap();
        }
    }

    pub fn wait_for_next_cycle(&self) {
        let (lock, cvar) = &*self.cycle_condvar;
        let mut next_cycle = lock.lock().unwrap();

        *next_cycle = false;
        while !*next_cycle {
            next_cycle = cvar.wait(next_cycle).unwrap();
        }
    }

    pub fn wait_for_ready(self) -> Self {
        {
            let (lock, cvar) = &*self.ready_condvar;
            let mut ready = lock.lock().unwrap();

            *ready = false;
            while !*ready {
                ready = cvar.wait(ready).unwrap();
            }
        }
        self
    }


    fn get_reg_addr_range(&self, slave_id: u16, register: &String, index: usize) -> Range<usize> {
        let slave_pos = SlavePos::from(slave_id);

        let (_pdo_entry_idx, bit_len, offset) = self.offsets[&slave_pos][register][index];
        let addr = offset.byte;
        let bytes_len = (bit_len / 8) as usize;

        addr..addr + bytes_len
    }

    fn get_reg_addr_ranges(&self, slave_id: u16, register: &String) -> Vec<Range<usize>> {
        let slave_pos = SlavePos::from(slave_id);

        let pdos = self.offsets[&slave_pos][register].clone();

        let mut ranges = Vec::new();
        for (pdo, bit_len, offset) in pdos {
            let addr = offset.byte;
            let bytes_len = (bit_len / 8) as usize;
            ranges.push(addr..addr + bytes_len);
        }
        ranges
    }

    pub fn get_slave_name(&self, slave_id: u16) -> Option<String> {
        self.slave_names
            .iter()
            .find(|(_, id)| u16::from(**id) == slave_id)
            .map(|(name, _)| name.clone())
    }

    pub fn get_slave_id(&self, slave_name: &String) -> Option<u16> {
        self.slave_names.get(slave_name).map(|id| u16::from(*id))
    }
}

type PdoOffsets = HashMap<String, Vec<(PdoEntryIdx, u8, Offset)>>;
type SlaveOffsets = HashMap<SlavePos, PdoOffsets>;
type SlaveNames = HashMap<String, SlavePos>;

pub fn init_master(
    idx: u32,
) -> Result<(Master, DomainIdx, SlaveOffsets, SlaveNames), io::Error> {

    let mut master = Master::open(idx, MasterAccess::ReadWrite)?;
    log::debug!("Reserve master");
    master.reserve()?;
    log::debug!("Create domain");
    let domain_idx = master.create_domain()?;
    let mut offsets: SlaveOffsets = HashMap::new();
    let mut slave_names:SlaveNames = HashMap::new();


    let slave_num = master.get_info().unwrap().slave_count;
    log::info!("Found {:?} slaves", slave_num);

    for i in 0..slave_num {
        let slave_info = master.get_slave_info(SlavePos::from(i as u16)).unwrap();
        log::info!("Slave {:?} at position {:?}", slave_info.name, i);
        slave_names.insert(slave_info.name.clone(), SlavePos::from(i as u16));
        log::debug!("Found device {:?}", slave_info);
        log::debug!("Vendor ID: {:X}, Product Code: {:X}, SM count {:?}", slave_info.id.vendor_id, slave_info.id.product_code, slave_info.sync_count);
        let slave_addr = SlaveAddr::ByPos(i as u16);
        let slave_id = SlaveId {
            vendor_id: slave_info.id.vendor_id,
            product_code: slave_info.id.product_code,
        };
        let pdos : Vec<PdoCfg> = (0..slave_info.sync_count).map(|j| {
            let sm_idx = SmIdx::new(j);
            let sm_info = master.get_sync(SlavePos::from(i as u16), sm_idx).unwrap();
            log::debug!("Found sm {:?}, pdo_count {:?}", sm_info, sm_info.pdo_count);
            
            if sm_info.pdo_count != 1 {
                log::error!("Only support 1 pdo per sync manager");
            }

            let pdo_cfg: PdoCfg = {
                let pdo_info = master.get_pdo(SlavePos::from(i as u16), sm_idx, PdoPos::new(0)).unwrap();
                log::debug!("Found pdo {:?}, entry_count {:?}", pdo_info, pdo_info.entry_count);

                let pdo_entries = (0..pdo_info.entry_count).map(|e| {
                    let entry_info = master.get_pdo_entry(SlavePos::from(i as u16), sm_idx, PdoPos::new(0), PdoEntryPos::new(e)).unwrap();
                    log::debug!("Found entry {:?}, bit_len {:?}", entry_info, entry_info.bit_len);
                    PdoEntryInfo {
                        entry_idx: entry_info.entry_idx,
                        bit_len: entry_info.bit_len as u8,
                        name: entry_info.name.clone(),
                        pos: PdoEntryPos::from(e as u8),
                    }
                }).collect();
                PdoCfg {
                    idx: PdoIdx::new(pdo_info.idx.into()),
                    entries: pdo_entries,
                }
            };
            pdo_cfg
        }).collect();
    
        let mut config = master.configure_slave(slave_addr, slave_id)?;

        let mut entry_offsets: PdoOffsets = HashMap::new();

        let mut pdo_idx = 0;
        for pdo in &pdos {
            if pdo_idx == 0 {
                config.config_sm_pdos(SmCfg::output(pdo_idx.into()), &[pdo.clone()])?;
                // Positions of TX PDO
                log::debug!("Positions of TX PDO 0x{:X}:", u16::from(pdo.idx));
            }else{
                config.config_sm_pdos(SmCfg::input(pdo_idx.into()), &[pdo.clone()])?;
                // Positions of RX PDO
                log::debug!("Positions of RX PDO 0x{:X}:", u16::from(pdo.idx));
            }
            for entry in &pdo.entries {
                let offset = config.register_pdo_entry(entry.entry_idx, domain_idx)?;
                let name = entry.name.clone();
                if entry_offsets.contains_key(&name) {
                    entry_offsets.get_mut(&name).unwrap().push((
                        entry.entry_idx,
                        entry.bit_len,
                        offset,
                    ));
                } else {
                    entry_offsets.insert(name, vec![(entry.entry_idx, entry.bit_len, offset)]);
                }
            }
            pdo_idx += 1;
        }
        
        let cfg_index = config.index();

        let cfg_info = master.get_config_info(cfg_index)?;
        log::debug!("Config info: {:#?}", cfg_info);
        if cfg_info.slave_position.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "Unable to configure slave",
            ));
        }
        offsets.insert(SlavePos::new(i as u16), entry_offsets);
    }

    Ok((master, domain_idx, offsets, slave_names))
}
